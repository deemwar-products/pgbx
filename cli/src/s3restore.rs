//! New-server disaster path: list and restore per-database dumps straight from S3, WITHOUT the extension
//! on the target. Layout (written by the extension): s3://<bucket>/<server>/<db>/<UTC timestamp>.dump
//!
//!   pgbx backups    --from-s3 --db shop <s3 flags>
//!   pgbx db-restore --from-s3 --db shop --into shop [--backup KEY | --time TS] <s3 flags>
//!
//! The dump is streamed from S3 into pg_restore (no temp file); a dropped connection resumes from the byte
//! reached (HTTP Range), so pg_restore never starts over. Credentials are read from a file and never printed.
//! Encrypted dumps (pgbx.encryption_key_file on the source) are decrypted in the stream with --key-file;
//! --with-roles first replays the backup's roles file (<ts>.globals.sql.zst; see src/globals.rs).

use crate::{check_time, pe, Args, Ctx, Out};
use chrono::{DateTime, NaiveDateTime, Utc};
use s3::{creds::Credentials, Bucket, Region};
use serde_json::json;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

/// The extension's schema names whose objects pg_restore skips when the target cannot install that extension.
const EXT_NAMES: [&str; 1] = ["pgbx"];

pub struct S3Flags {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub server: String,
    pub credentials_file: String,
}

pub fn s3_flags(a: &Args) -> Result<S3Flags, String> {
    let need = |k: &str| a.get(k).filter(|v| !v.is_empty()).map(String::from).ok_or(format!("--{k} is required with --from-s3"));
    Ok(S3Flags {
        endpoint: need("s3-endpoint")?,
        bucket: need("s3-bucket")?,
        region: a.get("s3-region").filter(|v| !v.is_empty()).unwrap_or("us-east-1").to_string(),
        server: need("server-name")?,
        credentials_file: need("credentials-file")?,
    })
}

/// (access_key_id, secret_access_key) from a credentials file. Values are never printed.
pub fn parse_credentials(s: &str) -> Result<(String, String), String> {
    let get = |k: &str| {
        s.lines()
            .find_map(|l| l.trim().strip_prefix(k).map(|v| v.trim_start().trim_start_matches('=').trim().to_string()))
            .filter(|v| !v.is_empty())
    };
    match (get("access_key_id"), get("secret_access_key")) {
        (Some(a), Some(b)) => Ok((a, b)),
        _ => Err("credentials file needs access_key_id= and secret_access_key= lines".into()),
    }
}

fn bucket(f: &S3Flags) -> Result<Box<Bucket>, String> {
    let text = std::fs::read_to_string(&f.credentials_file).map_err(|e| format!("read {}: {e}", f.credentials_file))?;
    let (ak, sk) = parse_credentials(&text)?;
    let creds = Credentials::new(Some(&ak), Some(&sk), None, None, None).map_err(|e| format!("credentials: {e}"))?;
    let b = Bucket::new(&f.bucket, Region::Custom { region: f.region.clone(), endpoint: f.endpoint.clone() }, creds)
        .map_err(|e| format!("bucket: {e}"))?;
    Ok(b.with_path_style())
}

/// A database name used as an S3 folder / new database: no slashes, quotes or control characters.
pub fn check_name(what: &str, n: &str) -> Result<(), String> {
    if n.is_empty() || n.len() > 63 || n.contains('/') || n.contains('"') || n.chars().any(|c| c.is_control()) {
        return Err(format!("{what} '{n}' is not a usable database name"));
    }
    Ok(())
}

pub fn prefix(server: &str, db: &str) -> String {
    format!("{server}/{db}/")
}

/// When a dump was taken, from its name: <prefix><YYYY-MM-DDTHH-MM-SSZ>.dump
pub fn key_time(key: &str) -> Option<DateTime<Utc>> {
    let stem = key.rsplit('/').next()?.strip_suffix(".dump")?;
    NaiveDateTime::parse_from_str(stem, "%Y-%m-%dT%H-%M-%SZ").ok().map(|n| n.and_utc())
}

/// --time with an explicit UTC offset ('2026-01-31 14:00:00+00', '...T14:00:00Z', '... 14:00+05:30').
pub fn parse_time(t: &str) -> Result<DateTime<Utc>, String> {
    check_time(t)?;
    let t = t.trim();
    let norm = match t.strip_suffix(['Z', 'z']) {
        Some(x) => format!("{x}+00:00"),
        None => t.to_string(),
    }
    .replacen('T', " ", 1);
    for f in ["%Y-%m-%d %H:%M:%S%.f%#z", "%Y-%m-%d %H:%M:%S%#z", "%Y-%m-%d %H:%M%#z"] {
        if let Ok(d) = DateTime::parse_from_str(&norm, f) {
            return Ok(d.with_timezone(&Utc));
        }
    }
    Err(format!("--time '{t}' could not be read; write it like '2026-01-31 14:00:00+00'"))
}

/// Which dump to restore. `keys` may be in any order; only *.dump keys with a readable timestamp count.
/// --backup: that exact key (or its file name) must be in the list. Otherwise: newest at or before `at` (default: newest).
pub fn pick_key(keys: &[String], backup: Option<&str>, at: Option<DateTime<Utc>>) -> Result<String, String> {
    let mut dumps: Vec<(DateTime<Utc>, &String)> = keys.iter().filter_map(|k| key_time(k).map(|t| (t, k))).collect();
    dumps.sort();
    if let Some(b) = backup {
        if at.is_some() {
            return Err("give --backup or --time, not both".into());
        }
        return dumps
            .iter()
            .find(|(_, k)| k.as_str() == b || k.rsplit('/').next() == Some(b))
            .map(|(_, k)| (*k).clone())
            .ok_or(format!("no backup '{b}' in this folder (list them with pgbx backups --from-s3)"));
    }
    if dumps.is_empty() {
        return Err("no backups in this folder (check --server-name and --db; list them with pgbx backups --from-s3)".into());
    }
    match at {
        None => Ok(dumps.last().unwrap().1.clone()),
        Some(at) => dumps
            .iter()
            .rev()
            .find(|(t, _)| *t <= at)
            .map(|(_, k)| (*k).clone())
            .ok_or(format!("no backup at or before {at} (the oldest is from {})", dumps[0].0)),
    }
}

/// The new database must not exist yet: a restore never overwrites anything.
pub fn check_target(into: &str, exists: bool) -> Result<(), String> {
    check_name("--into", into)?;
    if exists {
        return Err(format!("refusing: database '{into}' already exists on the target; pick a new name (nothing is ever overwritten)"));
    }
    Ok(())
}

/// pg_restore keeps going past errors and exits 1. When the target cannot install the extension, the only
/// errors allowed are about that extension (CREATE EXTENSION / COMMENT ON EXTENSION). Anything else fails.
pub fn only_extension_errors(stderr: &str) -> bool {
    let mut saw = false;
    for l in stderr.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let lower = l.to_lowercase();
        let about_ext = EXT_NAMES.iter().any(|n| lower.contains(&format!("extension \"{n}\"")) || lower.contains(&format!("extension {n}")))
            || EXT_NAMES.iter().any(|n| lower.contains(&format!("/{n}.control")));
        if about_ext {
            saw = true;
        } else if lower.contains("error") && !lower.starts_with("pg_restore: warning: errors ignored on restore") {
            return false;
        }
    }
    saw
}

fn list(b: &Bucket, pfx: &str) -> Result<Vec<(String, u64)>, String> {
    let pages = b.list(pfx.to_string(), None).map_err(|e| format!("list s3://{}/{pfx}: {e}", b.name()))?;
    Ok(pages.into_iter().flat_map(|p| p.contents.into_iter().map(|o| (o.key, o.size))).filter(|(k, _)| k.ends_with(".dump")).collect())
}

/// `pgbx backups --from-s3`: every dump of one database, newest first.
pub fn backups(cx: &mut Ctx) -> Out {
    let f = s3_flags(&cx.a)?;
    let db = cx.a.get("db").ok_or("--db NAME (the database's folder in S3) is required")?.to_string();
    check_name("--db", &db)?;
    let b = bucket(&f)?;
    let pfx = prefix(&f.server, &db);
    let mut v = list(&b, &pfx)?;
    v.sort_by(|x, y| y.0.cmp(&x.0));
    let rows: Vec<_> = v
        .iter()
        .map(|(k, n)| json!({"key": k, "taken_at": key_time(k).map(|t| t.to_rfc3339()), "bytes": n}))
        .collect();
    Ok(json!({"ok": true, "prefix": format!("s3://{}/{pfx}", f.bucket), "count": rows.len(), "backups": rows}))
}

/// Newest installed pg_restore (newer clients restore dumps from older servers): /usr/lib/postgresql/*/bin,
/// /usr/pgsql-*/bin, then PATH.
fn pg_restore_bin() -> PathBuf {
    let mut best: (PathBuf, u32) = (PathBuf::from("pg_restore"), 0);
    for root in ["/usr/lib/postgresql", "/usr"] {
        let Ok(rd) = std::fs::read_dir(root) else { continue };
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            let ver = if root == "/usr" { n.strip_prefix("pgsql-").map(String::from) } else { Some(n) };
            let (Some(ver), p) = (ver, e.path().join("bin/pg_restore")) else { continue };
            let major: u32 = ver.split('.').next().and_then(|x| x.parse().ok()).unwrap_or(0);
            if p.is_file() && major > best.1 {
                best = (p, major);
            }
        }
    }
    best.0
}

/// Writer wrapper that remembers whether the *destination* (pg_restore) failed, as opposed to the network.
struct Tracked<'a, W: Write> {
    inner: &'a mut W,
    written: &'a mut u64,
    dest_failed: &'a mut bool,
}
impl<W: Write> Write for Tracked<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self.inner.write(buf) {
            Ok(k) => {
                *self.written += k as u64;
                Ok(k)
            }
            Err(e) => {
                *self.dest_failed = true;
                Err(e)
            }
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

const BACKOFF_SECS: [u64; 8] = [1, 2, 4, 8, 16, 30, 30, 30];

/// Download `key` into `w`, resuming from the last byte on network errors (same approach as the extension).
fn download_resumable<W: Write + Send>(b: &Bucket, key: &str, w: &mut W) -> Result<u64, String> {
    let (head, code) = b.head_object(key).map_err(|e| format!("stat {key}: {e}"))?;
    if code / 100 != 2 {
        return Err(format!("stat {key}: HTTP {code}"));
    }
    let size = head.content_length.map(|n| n as u64).ok_or("no content length")?;
    let (mut done, mut dest_failed, mut last) = (0u64, false, String::new());
    for wait in std::iter::once(0).chain(BACKOFF_SECS) {
        if wait > 0 {
            eprintln!("pgbx: download {key}: {last}; resuming at byte {done} in {wait}s");
            std::thread::sleep(Duration::from_secs(wait));
        }
        let from = done;
        let mut t = Tracked { inner: w, written: &mut done, dest_failed: &mut dest_failed };
        let r = b.get_object_range_to_writer(key, from, None, &mut t);
        if dest_failed {
            return Err(format!("pg_restore stopped reading after {done} bytes"));
        }
        match r {
            Ok(c) if c / 100 == 2 && done >= size => return Ok(done),
            Ok(c) if c / 100 == 2 => last = format!("connection ended at {done}/{size} bytes"),
            Ok(c) => last = format!("HTTP {c} at {done}/{size} bytes"),
            Err(e) => last = format!("{e} at {done}/{size} bytes"),
        }
    }
    Err(format!("download {key}: gave up after {} retries: {last}", BACKOFF_SECS.len()))
}

fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// `pgbx db-restore --from-s3`: newest dump <= --time (or --backup KEY) -> CREATE DATABASE --into -> pg_restore.
pub fn db_restore(cx: &mut Ctx) -> Out {
    let f = s3_flags(&cx.a)?;
    let db = cx.a.get("db").ok_or("--db NAME (the database's folder in S3) is required")?.to_string();
    check_name("--db", &db)?;
    let into = cx.a.get("into").ok_or("--into NEWDB is required (a database that does not exist yet on the target)")?.to_string();
    let at = cx.a.get("time").map(parse_time).transpose()?;
    let backup = cx.a.get("backup").map(String::from);
    if backup.is_some() && at.is_some() {
        return Err("give --backup or --time, not both".into());
    }
    // encrypted dumps (pgbx.encryption_key_file on the source) are decrypted in the stream; plain ones pass through
    let key_file = cx.a.get("key-file").map(crate::crypt::Key::load).transpose()?;
    let roles_scope = match (cx.a.has("with-roles"), cx.a.get("roles")) {
        (false, Some(_)) => return Err("--roles works with --with-roles".into()),
        (false, None) => None,
        (true, r) => Some(crate::check_roles_scope(r)?),
    };

    let mut admin = cx.connect("postgres")?;
    let exists: bool = admin.query_one("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)", &[&into]).map_err(pe)?.get(0);
    check_target(&into, exists)?;
    let available: Vec<String> = admin
        .query("SELECT name FROM pg_available_extensions WHERE name = ANY($1)", &[&EXT_NAMES.iter().map(|s| s.to_string()).collect::<Vec<_>>()])
        .map_err(pe)?
        .iter()
        .map(|r| r.get(0))
        .collect();

    let b = bucket(&f)?;
    let keys: Vec<String> = list(&b, &prefix(&f.server, &db))?.into_iter().map(|(k, _)| k).collect();
    let key = pick_key(&keys, backup.as_deref(), at)?;

    // roles first (before CREATE DATABASE, so a failure leaves nothing half-made); existing roles are never changed
    let roles_report = match &roles_scope {
        Some(scope) => {
            let gkey = crate::globals::globals_key(&key);
            let mut z = zstd::stream::write::Decoder::new(Vec::new()).map_err(|e| format!("zstd: {e}"))?;
            {
                let mut d = crate::crypt::DecryptWriter::new(&mut z, key_file.as_ref(), true);
                let r = download_resumable(&b, &gkey, &mut d);
                if let Some(e) = d.error.clone() {
                    return Err(format!("roles file {gkey}: {e}"));
                }
                r.map_err(|e| format!("roles file {gkey} (backups taken before pgbx 0.6 have none; restore without --with-roles): {e}"))?;
                d.finish().map_err(|e| format!("roles file {gkey}: {e}"))?;
            }
            z.flush().map_err(|e| format!("zstd: {e}"))?;
            let sql = String::from_utf8(z.into_inner()).map_err(|_| "roles file is not text")?;
            let rep = crate::globals::apply(&mut admin, &sql, scope)?;
            Some(serde_json::from_str::<serde_json::Value>(&rep).map_err(|e| e.to_string())?)
        }
        None => None,
    };

    admin
        .batch_execute(&format!("CREATE DATABASE {} TEMPLATE template0", quote_ident(&into)))
        .map_err(|e| format!("create database {into}: {}", pe(e)))?;
    drop(admin);

    let (host, port, user) = cx.target()?;
    let port = port.to_string();
    let skipped: Vec<&str> = EXT_NAMES.iter().copied().filter(|n| !available.iter().any(|a| a == n)).collect();
    let exe = pg_restore_bin();
    let mut child = Command::new(&exe)
        .args(roles_scope.is_none().then_some("--no-owner")) // with roles: keep the owners
        .args(["-h", &host, "-p", &port, "-U", &user, "-d", &into])
        .args(skipped.iter().map(|n| format!("--exclude-schema={n}")))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run {}: {e} (install the PostgreSQL client tools)", exe.display()))?;
    let mut stdin = child.stdin.take().unwrap();
    let mut dw = crate::crypt::DecryptWriter::new(&mut stdin, key_file.as_ref(), true); // plain dumps pass through
    let dl = download_resumable(&b, &key, &mut dw);
    let (crypt_err, encrypted) = (dw.error.clone(), dw.encrypted() == Some(true));
    let fin = dw.finish().map(|_| ());
    drop(stdin); // EOF for pg_restore
    if let Some(e) = crypt_err.or(fin.err().filter(|_| encrypted).map(|e| e.to_string())) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("{key}: {e} (database {into} was created and is incomplete; drop it before retrying)"));
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let bytes = dl?;
    let mut warnings: Vec<String> = vec![];
    if !out.status.success() {
        if !skipped.is_empty() && only_extension_errors(&stderr) {
            warnings.push("the target cannot install the pgbx extension: its objects were skipped, your data is restored".into());
        } else {
            return Err(format!("pg_restore failed (database {into} was created and is incomplete; drop it before retrying): {}", stderr.trim()));
        }
    }
    // pg_restore still creates the (now empty) schema of a skipped extension: remove it, never anything inside
    if !skipped.is_empty() {
        if let Ok(mut c) = cx.connect(&into) {
            for n in &skipped {
                let _ = c.batch_execute(&format!("DROP SCHEMA IF EXISTS {} RESTRICT", quote_ident(n)));
            }
        }
    }
    Ok(json!({
        "ok": true, "restored_into": into, "source_db": db, "key": key, "taken_at": key_time(&key).map(|t| t.to_rfc3339()),
        "bytes": bytes, "encrypted": encrypted, "roles": roles_report, "warnings": warnings,
        "next": format!("check the data (psql -d {into}), then point your application at it"),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> Vec<String> {
        ["srv/shop/2026-01-01T02-00-00Z.dump", "srv/shop/2026-01-03T02-00-00Z.dump", "srv/shop/2026-01-02T02-00-00Z.dump",
         "srv/shop/notes.txt", "srv/shop/garbage.dump"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn picks_newest_at_or_before() {
        let k = keys();
        assert_eq!(pick_key(&k, None, None).unwrap(), "srv/shop/2026-01-03T02-00-00Z.dump");
        let at = parse_time("2026-01-02 12:00:00+00").unwrap();
        assert_eq!(pick_key(&k, None, Some(at)).unwrap(), "srv/shop/2026-01-02T02-00-00Z.dump");
        let exact = parse_time("2026-01-02T02:00:00Z").unwrap();
        assert_eq!(pick_key(&k, None, Some(exact)).unwrap(), "srv/shop/2026-01-02T02-00-00Z.dump");
        // offset honoured: 03:30+05:30 = 22:00 UTC the day before
        let ist = parse_time("2026-01-03 03:30+05:30").unwrap();
        assert_eq!(pick_key(&k, None, Some(ist)).unwrap(), "srv/shop/2026-01-02T02-00-00Z.dump");
        let early = parse_time("2025-12-31 00:00:00+00").unwrap();
        assert!(pick_key(&k, None, Some(early)).unwrap_err().contains("no backup at or before"));
        assert!(pick_key(&[], None, None).unwrap_err().contains("no backups"));
    }

    #[test]
    fn explicit_backup() {
        let k = keys();
        assert_eq!(pick_key(&k, Some("srv/shop/2026-01-01T02-00-00Z.dump"), None).unwrap(), "srv/shop/2026-01-01T02-00-00Z.dump");
        assert_eq!(pick_key(&k, Some("2026-01-01T02-00-00Z.dump"), None).unwrap(), "srv/shop/2026-01-01T02-00-00Z.dump");
        assert!(pick_key(&k, Some("srv/other/2026-01-01T02-00-00Z.dump"), None).unwrap_err().contains("no backup"));
        assert!(pick_key(&k, Some("srv/shop/notes.txt"), None).is_err());
        assert!(pick_key(&k, Some("x"), Some(Utc::now())).unwrap_err().contains("not both"));
    }

    #[test]
    fn time_must_carry_offset() {
        assert!(parse_time("2026-01-02 12:00:00").unwrap_err().contains("UTC offset"));
        assert!(parse_time("yesterday").is_err());
        assert_eq!(parse_time("2026-01-02 12:00:00+0200").unwrap().to_rfc3339(), "2026-01-02T10:00:00+00:00");
    }

    #[test]
    fn refuses_existing_or_bad_target() {
        assert!(check_target("shop", false).is_ok());
        assert!(check_target("shop", true).unwrap_err().contains("already exists"));
        assert!(check_target("", false).is_err());
        assert!(check_target("a/b", false).is_err());
        assert!(check_target("a\"b", false).is_err());
        assert!(check_name("--db", "../x").is_err());
    }

    #[test]
    fn s3_flags_required_and_credentials_parsed() {
        let a = crate::parse_args(["db-restore", "--from-s3", "--s3-endpoint", "http://s3", "--s3-bucket", "b"].iter().map(|s| s.to_string())).unwrap();
        assert!(s3_flags(&a).err().unwrap().contains("--server-name"));
        let (k, s) = parse_credentials("# c\naccess_key_id = AK\nsecret_access_key=SK\n").unwrap();
        assert_eq!((k.as_str(), s.as_str()), ("AK", "SK"));
        assert!(parse_credentials("access_key_id=AK\n").is_err());
    }

    #[test]
    fn tolerates_only_extension_errors() {
        let ext = "pg_restore: error: could not execute query: ERROR:  extension \"pgbx\" is not available\n\
                   DETAIL:  Could not open extension control file \"/usr/share/postgresql/16/extension/pgbx.control\": No such file or directory.\n\
                   Command was: CREATE EXTENSION IF NOT EXISTS pgbx WITH SCHEMA pgbx;\n\
                   pg_restore: error: could not execute query: ERROR:  extension \"pgbx\" does not exist\n\
                   Command was: COMMENT ON EXTENSION pgbx IS 'x';\n\
                   pg_restore: warning: errors ignored on restore: 2\n";
        assert!(only_extension_errors(ext));
        let real = format!("{ext}pg_restore: error: could not execute query: ERROR:  relation \"orders\" already exists\n");
        assert!(!only_extension_errors(&real));
        assert!(!only_extension_errors("pg_restore: error: input file appears to be a text format dump\n"));
        assert!(!only_extension_errors(""));
    }
}
