//! How a command reaches Postgres (ADR 0003): pgbx has no connection code of its own. A connection is
//!   - direct: --host/--port/--user flags, PGHOST/PGPORT/PGUSER, built-in defaults (no profile), or
//!   - a connection string: --url, PGBX_URL or a profile's `url:` ($VARs expanded at run time), or
//!   - an adapter: a profile's `adapter:` command, started on first use (see adapter.rs).
//!
//! The resolved URL lives in memory only. Its password is registered for redaction, handed to postgres in memory
//! and to child tools (pg_restore) through PGPASSWORD, never argv. A URL without a password falls back to PGPASSWORD.

use crate::adapter;
use crate::vars::{self, Resolver, Source};
use postgres::{Client, NoTls};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub enum Spec {
    Direct { host: String, port: String, user: String },
    Url { profile: Option<String>, raw: String },
    Adapter { profile: String, adapter: String, argv: Vec<String>, config: Value, ready_timeout: Duration },
}

/// A resolved connection: the postgres config (password included, in memory only) and the adapter, if any.
pub struct Live {
    pub cfg: postgres::Config,
    /// host:port for messages (never the password)
    pub display: String,
    adapter: Option<adapter::Handle>,
}

impl Drop for Live {
    fn drop(&mut self) {
        if let Some(h) = &self.adapter {
            h.stop();
        }
    }
}

type Resolved = Result<Arc<Live>, String>;

pub struct Conn {
    pub spec: Spec,
    source: Source,
    dir: PathBuf,
    live: Mutex<Option<(Instant, Resolved)>>,
}

impl std::fmt::Debug for Conn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Conn({})", self.memory_name()) // never the connection string
    }
}

/// A failed start is remembered this long, so one command does not start a failing adapter twice.
const RETRY_AFTER: Duration = Duration::from_secs(10);

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|s| !s.is_empty())
}

/// Register the password of a connection string (URL or key=value) for redaction.
pub fn register_url_secrets(url: &str) {
    if let Ok(c) = url.parse::<postgres::Config>() {
        if let Some(p) = c.get_password() {
            vars::register(&String::from_utf8_lossy(p));
        }
    }
}

/// `postgres://user:***@host:port/db` for display.
pub fn redact(url: &str) -> String {
    vars::redact_conn(url)
}

/// The host part of a connection string, without resolving anything (for memory folder names).
pub fn host_of(raw: &str) -> Option<String> {
    let rest = raw.split_once("://")?.1;
    let auth = &rest[..rest.find(['/', '?']).unwrap_or(rest.len())];
    let hp = auth.rsplit_once('@').map_or(auth, |x| x.1);
    let h = if hp.starts_with('[') { hp.split(']').next()?.trim_start_matches('[') } else { hp.split(':').next()? };
    (!h.is_empty() && !h.contains('$') && !h.contains('%')).then(|| h.to_string())
}

impl Conn {
    pub fn new(spec: Spec, source: Source, dir: PathBuf) -> Conn {
        Conn { spec, source, dir, live: Mutex::new(None) }
    }

    /// No profile, no url: the flags/env/defaults connection.
    pub fn direct(host: Option<&str>, port: Option<&str>, user: Option<&str>) -> Conn {
        let host = host.map(String::from).or(env("PGHOST")).unwrap_or(crate::DEFAULT_HOST.into());
        let port = port.map(String::from).or(env("PGPORT")).unwrap_or("5432".into());
        let user = user.map(String::from).or(env("PGUSER")).unwrap_or("postgres".into());
        Conn::new(Spec::Direct { host, port, user }, Source::Env, PathBuf::from("."))
    }

    pub fn profile(&self) -> Option<&str> {
        match &self.spec {
            Spec::Direct { .. } => None,
            Spec::Url { profile, .. } => profile.as_deref(),
            Spec::Adapter { profile, .. } => Some(profile),
        }
    }

    pub fn is_adapter(&self) -> bool {
        matches!(self.spec, Spec::Adapter { .. })
    }

    /// The folder name for agent memory: the profile, else the host.
    pub fn memory_name(&self) -> String {
        match &self.spec {
            Spec::Adapter { profile, .. } => profile.clone(),
            Spec::Url { profile: Some(p), .. } => p.clone(),
            Spec::Url { raw, .. } => host_of(raw).unwrap_or("localhost".into()),
            Spec::Direct { host, .. } => if host.starts_with('/') { "localhost".into() } else { host.clone() },
        }
    }

    /// Resolve once (expanding $VARs, starting the adapter); later calls reuse it.
    pub fn live(&self) -> Result<Arc<Live>, String> {
        let mut g = self.live.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((t, r)) = g.as_ref() {
            if r.is_ok() || t.elapsed() < RETRY_AFTER {
                return r.clone();
            }
        }
        let r = self.resolve().map(Arc::new).map_err(|e| vars::scrub(&e));
        *g = Some((Instant::now(), r.clone()));
        r
    }

    fn resolve(&self) -> Result<Live, String> {
        let mut res = Resolver::new(&env, self.source.clone(), self.dir.clone());
        let (mut cfg, display, adapter) = match &self.spec {
            Spec::Direct { host, port, user } => {
                let mut c = postgres::Config::new();
                let p: u16 = port.parse().map_err(|_| format!("bad --port '{port}'"))?;
                c.host(host).port(p).user(user);
                (c, format!("{host}:{port}"), None)
            }
            Spec::Url { profile, raw } => {
                let what = profile.as_ref().map_or("the connection string".into(), |p| format!("profile '{p}'"));
                let url = res.expand_url(raw).map_err(|e| format!("{what}: {e}"))?;
                register_url_secrets(&url);
                let c: postgres::Config = url.parse().map_err(|e| format!("{what}: not a valid connection string ({e})"))?;
                let d = display_of(&c);
                (c, d, None)
            }
            Spec::Adapter { profile, adapter: name, argv, config, ready_timeout } => {
                let cfg = res.expand_value(config).map_err(|e| format!("profile '{profile}': {e}"))?;
                let (run, ready) = adapter::Running::start(argv, profile, &cfg, *ready_timeout, Some(&self.dir))
                    .map_err(|e| format!("profile '{profile}' (adapter {name}): {e}"))?;
                let handle = adapter::keep(run);
                let c: postgres::Config = ready.url.parse()
                    .map_err(|e| format!("profile '{profile}': adapter {name} returned an invalid connection string ({e})"))?;
                let d = format!("{} via adapter {name}", display_of(&c));
                (c, d, Some(handle))
            }
        };
        if cfg.get_user().is_none() {
            cfg.user(env("PGUSER").as_deref().unwrap_or("postgres")); // pgbx's default user, as before 0.6
        }
        if cfg.get_password().is_none() {
            if let Some(pw) = env("PGPASSWORD") {
                vars::register(&pw);
                cfg.password(pw);
            }
        }
        if cfg.get_connect_timeout().is_none() {
            cfg.connect_timeout(Duration::from_secs(5));
        }
        cfg.application_name("pgbx");
        Ok(Live { cfg, display, adapter })
    }

    /// The database named in the connection string, if any (the default for --db).
    pub fn default_db(&self) -> Option<String> {
        match &self.spec {
            Spec::Direct { .. } => None,
            _ => self.live().ok()?.cfg.get_dbname().map(String::from),
        }
    }

    pub fn connect(&self, db: &str) -> Result<Client, String> {
        let live = self.live()?;
        let mut c = live.cfg.clone();
        c.dbname(db);
        c.connect(NoTls).map_err(|e| vars::scrub(&format!("cannot connect to Postgres ({}, db {db}): {e}", live.display)))
    }

    /// For child tools: (host, port, user, password). The password goes into their environment, never argv.
    pub fn tool_target(&self) -> Result<(String, u16, String, Option<String>), String> {
        let live = self.live()?;
        let c = &live.cfg;
        let host = match c.get_hosts().first() {
            Some(postgres::config::Host::Tcp(h)) => h.clone(),
            #[cfg(unix)]
            Some(postgres::config::Host::Unix(p)) => p.display().to_string(),
            None => crate::DEFAULT_HOST.to_string(),
        };
        let port = c.get_ports().first().copied().unwrap_or(5432);
        let user = c.get_user().unwrap_or("postgres").to_string();
        let pw = c.get_password().map(|p| String::from_utf8_lossy(p).to_string());
        Ok((host, port, user, pw))
    }

    /// Stop the adapter now (`pgbx serve` keeps it; a one-off command stops it at exit through adapter::stop_all).
    pub fn close(&self) {
        *self.live.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

fn display_of(c: &postgres::Config) -> String {
    let host = match c.get_hosts().first() {
        Some(postgres::config::Host::Tcp(h)) => h.clone(),
        #[cfg(unix)]
        Some(postgres::config::Host::Unix(p)) => p.display().to_string(),
        None => crate::DEFAULT_HOST.to_string(),
    };
    format!("{host}:{}", c.get_ports().first().copied().unwrap_or(5432))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_and_display() {
        assert_eq!(host_of("postgres://u:p@db.example:5432/x").as_deref(), Some("db.example"));
        assert_eq!(host_of("postgresql://db1/x").as_deref(), Some("db1"));
        assert_eq!(host_of("postgres://u@[::1]:5/x").as_deref(), Some("::1"));
        assert_eq!(host_of("postgres://$H/x"), None);
        assert_eq!(host_of("host=x"), None);
        assert_eq!(redact("postgres://u:pw@h/d"), "postgres://u:***@h/d");
    }

    #[test]
    fn url_spec_expands_and_keeps_the_password_in_memory() {
        std::env::set_var("PGBX_TEST_CONN_PW", "Conn-Test-Pw-9");
        let c = Conn::new(Spec::Url { profile: Some("dev".into()), raw: "postgres://app:$PGBX_TEST_CONN_PW@127.0.0.1:1/shop".into() },
            Source::Env, PathBuf::from("."));
        assert_eq!(c.default_db().as_deref(), Some("shop"));
        let (h, p, u, pw) = c.tool_target().unwrap();
        assert_eq!((h.as_str(), p, u.as_str(), pw.as_deref()), ("127.0.0.1", 1, "app", Some("Conn-Test-Pw-9")));
        let e = c.connect("other").err().unwrap();
        assert!(e.contains("127.0.0.1:1, db other") && !e.contains("Conn-Test-Pw-9"), "{e}");
        assert_eq!(c.memory_name(), "dev");
        let m = Conn::new(Spec::Url { profile: None, raw: "postgres://u:$NOT_SET_PGBX_X@h/d".into() }, Source::Env, PathBuf::from("."));
        let e = m.connect("d").err().unwrap();
        assert!(e.contains("$NOT_SET_PGBX_X is not set"), "{e}");
        assert_eq!(m.memory_name(), "h");
    }

    #[cfg(unix)]
    #[test]
    fn adapter_spec_starts_once_and_stops() {
        let d = std::env::temp_dir().join(format!("pgbx-conn-adp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("a.sh"), "#!/bin/sh\nread -r l\necho start >> \"$(dirname \"$0\")/starts\"\n\
            printf '{\"url\":\"postgres://ops:Adp-Pw-31@127.0.0.1:1/app\",\"state\":\"ready\",\"name\":\"prod\"}\\n'\n\
            while read -r l; do :; done\necho eof > \"$(dirname \"$0\")/eof\"\n").unwrap();
        std::env::set_var("PGBX_TEST_TARGET", "db1");
        let c = Conn::new(Spec::Adapter { profile: "prod".into(), adapter: "t".into(),
            argv: vec!["sh".into(), d.join("a.sh").display().to_string()],
            config: serde_json::json!({"target": "$PGBX_TEST_TARGET"}), ready_timeout: Duration::from_secs(5) },
            Source::Env, d.clone());
        assert_eq!(c.default_db().as_deref(), Some("app"));
        let e = c.connect("app").err().unwrap();
        assert!(e.contains("via adapter t") && !e.contains("Adp-Pw-31"), "{e}");
        let (_, _, _, pw) = c.tool_target().unwrap();
        assert_eq!(pw.as_deref(), Some("Adp-Pw-31"));
        assert_eq!(std::fs::read_to_string(d.join("starts")).unwrap().lines().count(), 1, "started once");
        c.close();
        assert!(d.join("eof").exists(), "close stops the adapter");
        let _ = std::fs::remove_dir_all(&d);
    }
}
