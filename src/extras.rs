//! pgbx 0.6 extras the job threads use: client-side encryption, the roles file kept with every backup, and
//! notifications. Nothing here reads a setting or calls into Postgres internals: the worker's main thread snapshots
//! the settings into `JobCfg` (see worker.rs), and job threads talk to databases as ordinary clients.

use crate::worker::{client_tool, log, pe, Ctx, JobCfg};
use crate::{crypt, globals, notify, transfer};
use pgrx::prelude::*;
use postgres::Client;
use s3::Bucket;
use std::io::{Read, Write};
use std::process::{Command, Stdio};

/// Internal: how many days a GFS spec reaches back; errors on a malformed spec. Used by set_retention().
#[pg_extern(immutable, strict, name = "_gfs_span")]
fn gfs_span(spec: &str) -> i32 {
    match crate::retention::Gfs::parse(spec) {
        Ok(Some(g)) => g.span_days() as i32,
        Ok(None) => 0,
        Err(e) => pgrx::error!("pgbx: {e}"),
    }
}

/// The configured encryption key (pgbx.encryption_key_file), if any. Loaded per job: one small file.
pub(crate) fn encryption_key(cfg: &JobCfg) -> Result<Option<crypt::Key>, String> {
    cfg.encryption_key_file
        .as_deref()
        .map(|f| crypt::Key::load(f).map_err(|e| format!("pgbx.encryption_key_file: {e}")))
        .transpose()
}

/// Upload `r` to `key`, through encryption when a key is set.
pub(crate) fn upload(
    b: &Bucket, key: &str, r: &mut impl Read, k: Option<&crypt::Key>, kbps: i32, progress: &mut dyn FnMut(u64),
) -> Result<u64, String> {
    match k {
        Some(k) => transfer::upload_stream(b, key, &mut crypt::EncryptReader::new(r, k), kbps, progress),
        None => transfer::upload_stream(b, key, r, kbps, progress),
    }
}

/// Download `key` into `w`, decrypting when the object is encrypted (plain objects pass through). Errors from the
/// decryption start with "decrypt" so the caller can report them instead of pg_restore's complaint about a cut input.
pub(crate) fn download<W: Write + Send>(
    cfg: &JobCfg, b: &Bucket, key: &str, w: &mut W, progress: &mut (dyn FnMut(u64) + Send),
) -> Result<u64, String> {
    let k = encryption_key(cfg)?;
    let mut d = crypt::DecryptWriter::new(w, k.as_ref(), true);
    let r = transfer::download_resumable(b, key, &mut d, cfg.download_kbps, progress);
    if let Some(e) = d.error.clone() {
        return Err(format!("decrypt {key}: {e}"));
    }
    r?;
    let n = d.plaintext_bytes;
    d.finish().map_err(|e| format!("decrypt {key}: {e}"))?;
    Ok(n)
}

/// Roles referenced by the database (owners, grantees, their parent roles) as a JSON array.
pub(crate) fn referenced_roles(cl: &mut Client) -> String {
    cl.query_one(globals::REFERENCED_ROLES_SQL, &[]).map(|r| r.get::<_, String>(0)).unwrap_or_else(|_| "[]".into())
}

/// pg_dumpall --globals-only -> zstd -> (encrypt) -> <ts>.globals.sql.zst. Streamed, never buffered.
/// Returns a JSON fragment for the backup's history params; a failure never fails the backup itself.
pub(crate) fn backup_globals(c: &Ctx, cfg: &JobCfg, b: &Bucket, dump_key: &str, roles_json: &str, k: Option<&crypt::Key>) -> String {
    let gkey = globals::globals_key(dump_key);
    let r = (|| -> Result<u64, String> {
        let mut cmd = Command::new(client_tool(c, "pg_dumpall").0);
        cmd.env("PGAPPNAME", "pgbx_dump")
            .args(["--globals-only", "-h", &c.socket, "-p", &c.port.to_string(), "-U", "postgres", "-l", &cfg.admin_db]);
        if !cfg.role_passwords {
            cmd.arg("--no-role-passwords");
        }
        let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| format!("spawn pg_dumpall: {e}"))?;
        let out = child.stdout.take().unwrap();
        let src = std::io::Cursor::new(globals::header_line(roles_json).into_bytes()).chain(out);
        let mut z = zstd::stream::read::Encoder::new(src, 3).map_err(|e| format!("zstd: {e}"))?;
        let up = upload(b, &gkey, &mut z, k, cfg.upload_kbps, &mut |_| {});
        drop(z);
        let res = child.wait_with_output().map_err(pe)?;
        if !res.status.success() {
            let _ = crate::s3auth::fresh(b).map(|b| b.delete_object(&gkey));
            return Err(format!("pg_dumpall: {}", String::from_utf8_lossy(&res.stderr).trim()));
        }
        up
    })();
    match r {
        Ok(_) => format!(",\"globals\":{},\"roles\":{roles_json}", globals::jstr(&gkey)),
        Err(e) => {
            log(&format!("roles file for {dump_key} not saved: {e}"));
            format!(",\"globals_error\":{}", globals::jstr(&e))
        }
    }
}

/// Download the roles file of `dump_key` and replay it on the server (see globals.rs). A small file: buffered.
pub(crate) fn restore_globals(cfg: &JobCfg, admin: &mut Client, b: &Bucket, dump_key: &str, scope: &str) -> Result<String, String> {
    let gkey = globals::globals_key(dump_key);
    let mut z = zstd::stream::write::Decoder::new(Vec::new()).map_err(|e| format!("zstd: {e}"))?;
    download(cfg, b, &gkey, &mut z, &mut |_| {})
        .map_err(|e| format!("roles file {gkey} (backups taken before 0.6 have none; restore without with_roles): {e}"))?;
    z.flush().map_err(|e| format!("zstd: {e}"))?;
    let sql = String::from_utf8(z.into_inner()).map_err(|_| "roles file is not text")?;
    globals::apply(admin, &sql, scope)
}

// ---------------------------------------------------------------- notifications

static DEDUP: std::sync::Mutex<Option<notify::Dedup>> = std::sync::Mutex::new(None);

/// Called (on the job's thread) for every finished backup / restore / restore test; `error` None = success.
/// One message per incident and one "OK again"; nothing when pgbx.notify is empty.
pub(crate) fn job_finished(cfg: &JobCfg, server: &str, db: &str, kind: &str, id: i64, error: Option<&str>) {
    if cfg.notify.is_none() || !["backup", "restore", "verify", "base_backup"].contains(&kind) {
        return;
    }
    let send = {
        let mut g = DEDUP.lock().unwrap_or_else(|e| e.into_inner());
        g.get_or_insert_with(Default::default).should_send(db, kind, error.is_some(), error.unwrap_or(""), std::time::Instant::now())
    };
    if send {
        send_event(cfg, notify::Event {
            failed: error.is_some(), server: server.into(), database: db.into(), kind: kind.into(), job_id: id,
            error: error.unwrap_or("").into(), at: chrono::Utc::now().to_rfc3339(),
        });
    }
}

/// Send one event to every pgbx.notify channel; failures go to the Postgres log (never with a URL or token).
pub(crate) fn send_event(cfg: &JobCfg, ev: notify::Event) {
    let Some(spec) = cfg.notify.as_deref() else { return };
    for e in notify::send_all(spec, cfg.notify_secrets_file.as_deref(), &ev) {
        log(&e);
    }
}



/// From the main thread: run pgbx.alert_command and the pgbx.notify channels for a whole-server incident on a
/// thread of their own, so a slow webhook or SMTP server never holds up the worker's poll loop.
pub(crate) fn incident_async(cfg: JobCfg, server: String, kind: &'static str, id: i64, msg: String, failed: bool) {
    let r = std::thread::Builder::new().name("pgbx notify".into()).spawn(move || {
        if failed || kind.ends_with("_recovered") {
            crate::worker::alert(cfg.alert_command.as_deref(), &server, "(whole server)", kind, id, &msg);
        }
        send_event(&cfg, notify::Event {
            failed, server, database: "(whole server)".into(), kind: kind.trim_end_matches("_recovered").into(), job_id: id,
            error: msg, at: chrono::Utc::now().to_rfc3339(),
        });
    });
    if let Err(e) = r {
        log(&format!("notify: could not start a thread: {e}"));
    }
}
