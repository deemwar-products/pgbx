//! `pgbx setup`: first-time server configuration after the installer put the files in place.
//! Writes ONE drop-in file `<config dir>/conf.d/pgbx.conf` (shared_preload_libraries MERGED with what is
//! already loaded, plus the pgbx.s3_* settings) and the S3 credentials file (0600, owned by the postgres OS
//! user). Never edits postgresql.conf except to add one `include_dir = 'conf.d'` line when it is missing, and never
//! restarts Postgres. Guarded: without --yes it only shows the plan.
#![cfg_attr(not(unix), allow(dead_code, unused_imports))]
use crate::{Ctx, Out};
use serde_json::json;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

pub const LIB: &str = "pgbx";
pub const DROPIN: &str = "pgbx.conf";
pub const DEFAULT_CREDS: &str = "/etc/pgbx/s3.credentials";

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Opts {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub server_name: String,
    pub credentials_file: String,
}

/// Split a shared_preload_libraries value ("a, 'b',c") into names.
pub fn split_libs(v: &str) -> Vec<String> {
    v.split(',').map(|s| s.trim().trim_matches(|c| c == '\'' || c == '"').trim().to_string()).filter(|s| !s.is_empty()).collect()
}

/// Existing libraries kept in order, duplicates dropped, pgbx appended once.
pub fn merge_preload(existing: &str) -> String {
    let mut out: Vec<String> = vec![];
    for l in split_libs(existing).into_iter().chain(std::iter::once(LIB.to_string())) {
        if !out.contains(&l) {
            out.push(l);
        }
    }
    out.join(",")
}

/// postgresql.conf string literal
pub fn quote(v: &str) -> String {
    format!("'{}'", v.replace('\\', "\\\\").replace('\'', "''"))
}

pub fn render_conf(o: &Opts, preload: &str) -> String {
    let mut s = String::from(
        "# Written by `pgbx setup` (pgbx). Re-running pgbx setup rewrites this file.\n\
         # shared_preload_libraries needs a Postgres restart; pgbx.* changes need only SELECT pg_reload_conf().\n",
    );
    let mut kv = vec![("shared_preload_libraries", preload.to_string())];
    kv.push(("pgbx.s3_endpoint", o.endpoint.clone()));
    kv.push(("pgbx.s3_bucket", o.bucket.clone()));
    if !o.region.is_empty() {
        kv.push(("pgbx.s3_region", o.region.clone()));
    }
    if !o.server_name.is_empty() {
        kv.push(("pgbx.server_name", o.server_name.clone()));
    }
    kv.push(("pgbx.credentials_file", o.credentials_file.clone()));
    for (k, v) in kv {
        s.push_str(&format!("{k} = {}\n", quote(&v)));
    }
    s
}

pub fn render_credentials(key: &str, secret: &str) -> String {
    format!("access_key_id={key}\nsecret_access_key={secret}\n")
}

fn strip_comment(l: &str) -> &str {
    // good enough for postgresql.conf: '#' outside quotes starts a comment
    let mut q = false;
    for (i, c) in l.char_indices() {
        match c {
            '\'' => q = !q,
            '#' if !q => return &l[..i],
            _ => {}
        }
    }
    l
}

/// Last uncommented `name = value` in a postgresql.conf text (value unquoted).
pub fn conf_value(text: &str, name: &str) -> Option<String> {
    let mut v = None;
    for l in text.lines() {
        let l = strip_comment(l).trim();
        let Some(rest) = l.strip_prefix(name) else { continue };
        let rest = rest.trim_start();
        let rest = rest.strip_prefix('=').unwrap_or(rest).trim();
        if rest.is_empty() || !(l.len() > name.len() && (l.as_bytes()[name.len()] == b' ' || l.as_bytes()[name.len()] == b'=' || l.as_bytes()[name.len()] == b'\t')) {
            continue;
        }
        let r = rest.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')).map(|r| r.replace("''", "'")).unwrap_or(rest.to_string());
        v = Some(r);
    }
    v
}

/// Does postgresql.conf (in `conf_dir`) include `<conf_dir>/conf.d`?
pub fn includes_conf_d(text: &str, conf_dir: &Path) -> bool {
    text.lines().any(|l| {
        let l = strip_comment(l).trim();
        let Some(rest) = l.strip_prefix("include_dir") else { return false };
        let rest = rest.trim_start();
        let v = rest.strip_prefix('=').unwrap_or(rest).trim().trim_matches('\'').trim_end_matches('/');
        v == "conf.d" || v == "./conf.d" || Path::new(v) == conf_dir.join("conf.d")
    })
}

/// The one command that applies shared_preload_libraries.
pub fn restart_command(config_file: &str, data_dir: &str) -> String {
    let parts: Vec<&str> = config_file.split('/').collect();
    // Debian/Ubuntu: /etc/postgresql/<ver>/<cluster>/postgresql.conf
    if let ["", "etc", "postgresql", ver, cluster, "postgresql.conf"] = parts.as_slice() {
        return format!("sudo systemctl restart postgresql@{ver}-{cluster}");
    }
    // RHEL/PGDG: /var/lib/pgsql/<ver>/data/postgresql.conf
    if let ["", "var", "lib", "pgsql", ver, "data", "postgresql.conf"] = parts.as_slice() {
        return format!("sudo systemctl restart postgresql-{ver}");
    }
    format!("sudo -u postgres pg_ctl -D {data_dir} restart")
}

fn prompt(label: &str, default: Option<&str>) -> Result<String, String> {
    let d = default.filter(|d| !d.is_empty());
    eprint!("{label}{}: ", d.map(|d| format!(" [{d}]")).unwrap_or_default());
    let _ = std::io::stderr().flush();
    let mut s = String::new();
    std::io::stdin().lock().read_line(&mut s).map_err(|e| e.to_string())?;
    let s = s.trim().to_string();
    Ok(if s.is_empty() { d.unwrap_or("").to_string() } else { s })
}

/// Read a secret from the terminal without echo (unix: stty).
fn prompt_secret(label: &str) -> Result<String, String> {
    eprint!("{label} (not echoed): ");
    let _ = std::io::stderr().flush();
    let tty = std::process::Command::new("stty").arg("-echo").stdin(std::process::Stdio::inherit()).status().map(|s| s.success()).unwrap_or(false);
    let mut s = String::new();
    let r = std::io::stdin().lock().read_line(&mut s);
    if tty {
        let _ = std::process::Command::new("stty").arg("echo").stdin(std::process::Stdio::inherit()).status();
    }
    eprintln!();
    r.map_err(|e| e.to_string())?;
    Ok(s.trim().to_string())
}

/// What the server looks like, from SQL when Postgres is up, else from --pg-conf.
#[derive(Debug, Default)]
pub struct Found {
    pub config_file: String,
    pub data_dir: String,
    pub preload: String,
    pub from: &'static str,
    pub major: Option<u32>,
}

/// The extension needs PostgreSQL 13-18.
pub fn check_major(major: Option<u32>) -> Result<(), String> {
    match major {
        Some(m) if !(13..=18).contains(&m) => Err(format!("pgbx's extension supports PostgreSQL 13–18; found {m}")),
        _ => Ok(()),
    }
}

/// Major version from a Debian/RHEL config path (/etc/postgresql/16/main/..., /var/lib/pgsql/17/data/...).
pub fn major_from_path(p: &str) -> Option<u32> {
    let parts: Vec<&str> = p.split('/').collect();
    parts.windows(2).find(|w| w[0] == "postgresql" || w[0] == "pgsql").and_then(|w| w[1].split('.').next()?.parse().ok())
}

fn find_server(cx: &mut Ctx) -> Result<Found, String> {
    if let Ok(mut c) = cx.connect("postgres") {
        let show = |c: &mut postgres::Client, n: &str| -> Result<String, String> {
            c.query_one(&format!("SHOW {n}"), &[]).map(|r| r.get::<_, String>(0)).map_err(|e| format!("SHOW {n}: {e}"))
        };
        return Ok(Found {
            config_file: show(&mut c, "config_file")?,
            data_dir: show(&mut c, "data_directory")?,
            preload: show(&mut c, "shared_preload_libraries")?,
            from: "sql",
            major: show(&mut c, "server_version_num").ok().and_then(|v| v.parse::<u32>().ok()).map(|n| if n >= 100000 { n / 10000 } else { n / 100 }),
        });
    }
    let conf = match cx.a.get("pg-conf") {
        Some(p) => p.to_string(),
        None => {
            let mut c: Vec<PathBuf> = ["/etc/postgresql", "/var/lib/pgsql"].iter().flat_map(|root| {
                std::fs::read_dir(root).into_iter().flatten().flatten().flat_map(|v| {
                    [v.path().join("main/postgresql.conf"), v.path().join("data/postgresql.conf")]
                })
            }).filter(|p| p.is_file()).collect();
            c.sort();
            match c.len() {
                1 => c[0].display().to_string(),
                0 => return Err("Postgres is not reachable and no postgresql.conf was found; start Postgres or pass --pg-conf FILE".into()),
                _ => return Err(format!("Postgres is not reachable and several clusters exist ({}); pass --pg-conf FILE",
                    c.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "))),
            }
        }
    };
    let text = std::fs::read_to_string(&conf).map_err(|e| format!("{conf}: {e}"))?;
    let dir = Path::new(&conf).parent().unwrap_or(Path::new("."));
    let mut preload = conf_value(&text, "shared_preload_libraries").unwrap_or_default();
    if let Some(v) = std::fs::read_to_string(dir.join("conf.d").join(DROPIN)).ok().and_then(|t| conf_value(&t, "shared_preload_libraries")) {
        preload = v; // our previous drop-in (read after postgresql.conf) wins
    }
    let data_dir = conf_value(&text, "data_directory").unwrap_or_else(|| dir.display().to_string());
    let major = major_from_path(&conf);
    Ok(Found { config_file: conf, data_dir, preload, from: "postgresql.conf", major })
}

#[cfg(unix)]
fn postgres_ids() -> Option<(u32, u32)> {
    let id = |f: &str| crate::sh_out("id", &[f, "postgres"])?.parse().ok();
    Some((id("-u")?, id("-g")?))
}

#[cfg(unix)]
fn write_file(p: &Path, body: &str, mode: u32, owner: Option<(u32, u32)>) -> Result<(), String> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let tmp = p.with_extension("pgbx-tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    f.write_all(body.as_bytes()).and_then(|_| f.sync_all()).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)).map_err(|e| e.to_string())?;
    if let Some((u, g)) = owner {
        std::os::unix::fs::chown(&tmp, Some(u), Some(g)).map_err(|e| format!("chown {}: {e} (run with sudo)", tmp.display()))?;
    }
    std::fs::rename(&tmp, p).map_err(|e| format!("{}: {e}", p.display()))
}

#[cfg(unix)]
fn ensure_dir(d: &Path, mode: u32, owner: Option<(u32, u32)>) -> Result<bool, String> {
    if d.is_dir() {
        return Ok(false);
    }
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(mode).create(d).map_err(|e| format!("create {}: {e} (run with sudo)", d.display()))?;
    if let Some((u, g)) = owner {
        std::os::unix::fs::chown(d, Some(u), Some(g)).map_err(|e| format!("chown {}: {e}", d.display()))?;
    }
    Ok(true)
}

#[cfg(not(unix))]
pub fn run(_cx: &mut Ctx) -> Out {
    Err("pgbx setup configures a Linux Postgres server and is not supported on Windows: run it on the database server".into())
}

#[cfg(unix)]
pub fn run(cx: &mut Ctx) -> Out {
    let interactive = !cx.a.has("json") && std::io::stdin().is_terminal();
    let env = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());
    let get = |flag: &str, label: &str, default: Option<String>, required: bool| -> Result<String, String> {
        if let Some(v) = cx.a.get(flag) {
            return Ok(v.to_string());
        }
        let v = if interactive { prompt(label, default.as_deref())? } else { default.unwrap_or_default() };
        if required && v.is_empty() {
            return Err(format!("--{flag} is required"));
        }
        Ok(v)
    };
    let host = crate::sh_out("hostname", &[]).unwrap_or_default();
    let mut o = Opts {
        endpoint: get("s3-endpoint", "S3 endpoint URL (e.g. https://s3.eu-central-1.amazonaws.com)", None, true)?,
        bucket: get("s3-bucket", "S3 bucket", None, true)?,
        region: get("s3-region", "S3 region", Some("us-east-1".into()), false)?,
        server_name: get("server-name", "Server name (folder in the bucket)", Some(host), false)?,
        credentials_file: String::new(),
    };
    o.credentials_file = get("credentials-file", "Credentials file", Some(DEFAULT_CREDS.into()), true)?;

    // keys: only from named env vars or a no-echo prompt; never from argv, never printed
    let key_env = cx.a.get("access-key-env").unwrap_or("AWS_ACCESS_KEY_ID").to_string();
    let sec_env = cx.a.get("secret-key-env").unwrap_or("AWS_SECRET_ACCESS_KEY").to_string();
    let creds_exist = Path::new(&o.credentials_file).exists();
    let mut keys = match (env(&key_env), env(&sec_env)) {
        (Some(k), Some(s)) => Some((k, s, format!("from ${key_env} / ${sec_env}"))),
        _ if cx.a.has("access-key-env") || cx.a.has("secret-key-env") => {
            return Err(format!("${key_env} and ${sec_env} must both be set (they are read, never printed)"))
        }
        _ => None,
    };
    let mut missing_creds = None;
    if keys.is_none() && !creds_exist {
        let msg = format!("{} does not exist: set {key_env}/{sec_env} (or name others with --access-key-env/--secret-key-env)", o.credentials_file);
        if !interactive && cx.a.has("yes") {
            return Err(msg);
        }
        if !interactive {
            missing_creds = Some(msg);
        }
    }
    if keys.is_none() && !creds_exist && interactive {
        let k = prompt_secret("S3 access key id")?;
        let s = prompt_secret("S3 secret access key")?;
        if k.is_empty() || s.is_empty() {
            return Err("both keys are needed".into());
        }
        keys = Some((k, s, "typed at the prompt".into()));
    }

    let found = find_server(cx)?;
    check_major(found.major)?;
    let conf_dir = Path::new(&found.config_file).parent().ok_or("config_file has no directory")?.to_path_buf();
    let conf_text = std::fs::read_to_string(&found.config_file).map_err(|e| format!("{}: {e} (run with sudo)", found.config_file))?;
    let need_include = !includes_conf_d(&conf_text, &conf_dir);
    let dropin = conf_dir.join("conf.d").join(DROPIN);
    let preload = merge_preload(&found.preload);
    let body = render_conf(&o, &preload);
    let restart = restart_command(&found.config_file, &found.data_dir);
    let mut warnings: Vec<String> = vec![];
    if let Ok(auto) = std::fs::read_to_string(Path::new(&found.data_dir).join("postgresql.auto.conf")) {
        if conf_value(&auto, "shared_preload_libraries").is_some() {
            warnings.push("postgresql.auto.conf (ALTER SYSTEM) sets shared_preload_libraries and overrides conf.d; run \
                ALTER SYSTEM RESET shared_preload_libraries or add pgbx there".into());
        }
    }
    if let Some(m) = missing_creds {
        warnings.push(m);
    }
    let creds_action = match &keys {
        Some((_, _, src)) => format!("write {} (0600, owner postgres; keys {src})", o.credentials_file),
        None => format!("keep existing {}", o.credentials_file),
    };
    let mut changes = vec![];
    if need_include {
        changes.push(format!("append `include_dir = 'conf.d'` to {} (it does not include conf.d yet)", found.config_file));
    }
    changes.push(format!("write {}", dropin.display()));
    changes.push(creds_action);
    let mut v = json!({
        "ok": true, "applied": false, "found_via": found.from,
        "config_file": found.config_file, "data_directory": found.data_dir, "conf_d_file": dropin.display().to_string(),
        "include_dir_added": false,
        "shared_preload_libraries": {"before": found.preload, "after": preload},
        "settings": {"pgbx.s3_endpoint": o.endpoint, "pgbx.s3_bucket": o.bucket, "pgbx.s3_region": o.region,
                     "pgbx.server_name": o.server_name, "pgbx.credentials_file": o.credentials_file},
        "credentials_file": {"path": o.credentials_file, "written": false},
        "changes": changes, "warnings": warnings, "restart": restart,
    });
    if !cx.a.has("yes") {
        v["next"] = json!("nothing written: re-run with --yes to apply these changes");
        return Ok(v);
    }

    let owner = postgres_ids();
    if crate::is_root() && owner.is_none() {
        return Err("no 'postgres' OS user found; the credentials file must be owned by the user Postgres runs as".into());
    }
    let owner = if crate::is_root() { owner } else { None };
    if need_include {
        let mut t = conf_text.clone();
        if !t.ends_with('\n') {
            t.push('\n');
        }
        t.push_str("include_dir = 'conf.d'\t\t\t# added by pgbx setup (pgbx)\n");
        std::fs::write(&found.config_file, t).map_err(|e| format!("{}: {e} (run with sudo)", found.config_file))?;
        v["include_dir_added"] = json!(true);
    }
    ensure_dir(&conf_dir.join("conf.d"), 0o755, owner)?;
    write_file(&dropin, &body, 0o644, owner)?;
    if let Some((k, s, _)) = &keys {
        if let Some(p) = Path::new(&o.credentials_file).parent() {
            ensure_dir(p, 0o750, owner)?;
        }
        write_file(Path::new(&o.credentials_file), &render_credentials(k, s), 0o600, owner)?;
        v["credentials_file"]["written"] = json!(true);
        v["credentials_file"]["mode"] = json!("0600");
        v["credentials_file"]["owner"] = json!(if owner.is_some() { "postgres" } else { "current user (not root)" });
    }
    v["applied"] = json!(true);
    v["next"] = json!(format!("restart Postgres once to load the extension: {restart} — then the worker creates the \
        extension in every database within seconds; check with: pgbx doctor"));
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_preload_without_losing_or_duplicating() {
        assert_eq!(merge_preload(""), "pgbx");
        assert_eq!(merge_preload("pg_stat_statements"), "pg_stat_statements,pgbx");
        assert_eq!(merge_preload("'pg_stat_statements', auto_explain"), "pg_stat_statements,auto_explain,pgbx");
        assert_eq!(merge_preload("pgbx,pg_stat_statements"), "pgbx,pg_stat_statements");
        assert_eq!(merge_preload("a, a ,pgbx"), "a,pgbx");
    }

    #[test]
    fn renders_dropin() {
        let o = Opts { endpoint: "https://s3.x".into(), bucket: "b".into(), region: "".into(), server_name: "db's".into(),
                       credentials_file: "/etc/pgbx/s3.credentials".into() };
        let s = render_conf(&o, "pg_stat_statements,pgbx");
        assert!(s.contains("shared_preload_libraries = 'pg_stat_statements,pgbx'\n"));
        assert!(s.contains("pgbx.s3_endpoint = 'https://s3.x'\n"));
        assert!(s.contains("pgbx.server_name = 'db''s'\n"), "quotes escaped");
        assert!(!s.contains("s3_region"), "empty region omitted");
        assert!(!s.contains("archive_mode") && !s.contains("cluster"), "per-database only");
        assert_eq!(conf_value(&s, "pgbx.server_name").as_deref(), Some("db's"));
        assert_eq!(conf_value(&s, "shared_preload_libraries").as_deref(), Some("pg_stat_statements,pgbx"));
    }

    #[test]
    fn reads_conf_values() {
        let t = "#shared_preload_libraries = 'x'\nshared_preload_libraries='a'  # c\nshared_preload_libraries_x = 1\n";
        assert_eq!(conf_value(t, "shared_preload_libraries").as_deref(), Some("a"));
        assert_eq!(conf_value("# nothing\n", "shared_preload_libraries"), None);
    }

    #[test]
    fn detects_conf_d_include() {
        let d = Path::new("/etc/postgresql/16/main");
        assert!(includes_conf_d("include_dir = 'conf.d'\t# ok\n", d));
        assert!(includes_conf_d("include_dir '/etc/postgresql/16/main/conf.d'\n", d));
        assert!(!includes_conf_d("#include_dir = 'conf.d'\n", d));
        assert!(!includes_conf_d("include_dir = 'other'\n", d));
    }

    #[test]
    fn restart_commands() {
        assert_eq!(restart_command("/etc/postgresql/16/main/postgresql.conf", "/var/lib/postgresql/16/main"), "sudo systemctl restart postgresql@16-main");
        assert_eq!(restart_command("/var/lib/pgsql/17/data/postgresql.conf", "/var/lib/pgsql/17/data"), "sudo systemctl restart postgresql-17");
        assert_eq!(restart_command("/srv/pg/postgresql.conf", "/srv/pg"), "sudo -u postgres pg_ctl -D /srv/pg restart");
    }

    #[test]
    fn version_gate() {
        assert_eq!(major_from_path("/etc/postgresql/16/main/postgresql.conf"), Some(16));
        assert_eq!(major_from_path("/var/lib/pgsql/9.6/data/postgresql.conf"), Some(9));
        assert_eq!(major_from_path("/srv/pg/postgresql.conf"), None);
        assert!(check_major(Some(12)).unwrap_err().contains("supports PostgreSQL 13–18; found 12"));
        assert!(check_major(Some(13)).is_ok() && check_major(None).is_ok());
    }

    #[test]
    fn credentials_format() {
        assert_eq!(render_credentials("AK", "SK"), "access_key_id=AK\nsecret_access_key=SK\n");
    }
}
