//! Named connection profiles: one per Postgres server the CLI talks to.
//!
//! Stored as JSON in `<config dir>/profiles.json` (0600): `~/.config/pgbx` (or `$XDG_CONFIG_HOME/pgbx`),
//! `%APPDATA%\pgbx` on Windows, `$PGBX_CONFIG_DIR` when set. Profiles hold connection and S3 *locations*
//! only — never a password or an S3 key (passwords: ~/.pgpass or PGPASSWORD; S3 keys: the credentials file).
//!
//! Precedence for every value: explicit flag > environment (PGHOST/PGPORT/PGUSER) > profile > built-in default.
//! The profile is chosen by `--profile NAME`, else `PGBX_PROFILE`, else the default set by `pgbx profile use`.

use crate::Args;
use serde_json::{json, Map, Value};
use std::path::PathBuf;

/// Flags a profile may hold, and the environment variable that beats the profile for each (if any).
pub const KEYS: &[(&str, Option<&str>)] = &[
    ("host", Some("PGHOST")),
    ("port", Some("PGPORT")),
    ("user", Some("PGUSER")),
    ("admin-db", None),
    ("s3-endpoint", None),
    ("s3-bucket", None),
    ("s3-region", None),
    ("server-name", None),
    ("credentials-file", None),
];

type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

fn sys_env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|s| !s.is_empty())
}

pub fn config_dir(env: Env) -> Result<PathBuf, String> {
    if let Some(d) = env("PGBX_CONFIG_DIR") {
        return Ok(PathBuf::from(d));
    }
    if cfg!(windows) {
        if let Some(a) = env("APPDATA") {
            return Ok(PathBuf::from(a).join("pgbx"));
        }
    }
    if let Some(x) = env("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(x).join("pgbx"));
    }
    let home = env("HOME").or_else(|| env("USERPROFILE")).ok_or("HOME (or USERPROFILE) is not set")?;
    Ok(PathBuf::from(home).join(".config").join("pgbx"))
}

#[derive(Debug, Default, PartialEq)]
pub struct Store {
    pub default: Option<String>,
    pub profiles: Map<String, Value>,
}

impl Store {
    pub fn from_json(s: &str) -> Result<Store, String> {
        let v: Value = serde_json::from_str(s).map_err(|e| format!("profiles file is not valid JSON: {e}"))?;
        Ok(Store {
            default: v["default"].as_str().map(String::from),
            profiles: v["profiles"].as_object().cloned().unwrap_or_default(),
        })
    }
    pub fn to_json(&self) -> Value {
        json!({"default": self.default, "profiles": self.profiles})
    }
}

fn path(env: Env) -> Result<PathBuf, String> {
    Ok(config_dir(env)?.join("profiles.json"))
}

fn load(env: Env) -> Result<Store, String> {
    let p = path(env)?;
    match std::fs::read_to_string(&p) {
        Ok(s) => Store::from_json(&s).map_err(|e| format!("{}: {e}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
        Err(e) => Err(format!("cannot read {}: {e}", p.display())),
    }
}

fn save(env: Env, st: &Store) -> Result<PathBuf, String> {
    let p = path(env)?;
    let dir = p.parent().unwrap();
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let body = serde_json::to_string_pretty(&st.to_json()).unwrap() + "\n";
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        o.mode(0o600);
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    use std::io::Write;
    let mut f = o.open(&p).map_err(|e| format!("cannot write {}: {e}", p.display()))?;
    #[cfg(unix)]
    {
        // an existing file keeps its old mode on open; tighten it
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    }
    f.write_all(body.as_bytes()).map_err(|e| format!("cannot write {}: {e}", p.display()))?;
    Ok(p)
}

pub fn check_name(n: &str) -> Result<(), String> {
    if !n.is_empty() && n.len() <= 64 && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.') {
        Ok(())
    } else {
        Err(format!("bad profile name '{n}': use letters, digits, '-', '_' or '.'"))
    }
}

/// Fill `a.flags` from the chosen profile where neither a flag nor an overriding env var is set.
/// Returns the profile name used, if any.
pub fn apply(a: &mut Args, st: &Store, env: Env) -> Result<Option<String>, String> {
    let explicit = a.flags.get("profile").cloned().or_else(|| env("PGBX_PROFILE"));
    let Some(name) = explicit.clone().or_else(|| st.default.clone()) else { return Ok(None) };
    let Some(p) = st.profiles.get(&name) else {
        return Err(match explicit {
            Some(_) => format!("no profile '{name}' (pgbx profile list)"),
            None => format!("default profile '{name}' no longer exists (pgbx profile use <name>)"),
        });
    };
    for (k, env_var) in KEYS {
        let Some(v) = p.get(*k).and_then(|v| v.as_str()) else { continue };
        if a.flags.contains_key(*k) || env_var.and_then(env).is_some() {
            continue;
        }
        a.flags.insert(k.to_string(), v.to_string());
    }
    Ok(Some(name))
}

/// Load the store and apply it (main's entry point).
pub fn apply_from_disk(a: &mut Args) -> Result<Option<String>, String> {
    let wants = a.flags.contains_key("profile") || sys_env("PGBX_PROFILE").is_some();
    match load(&sys_env) {
        Ok(st) => apply(a, &st, &sys_env),
        // a broken file only matters when a profile was asked for or set as default
        Err(e) if wants => Err(e),
        Err(_) => Ok(None),
    }
}

fn show(name: &str, st: &Store) -> Value {
    json!({"name": name, "default": st.default.as_deref() == Some(name), "settings": st.profiles[name]})
}

/// `pgbx profile add|list|show|remove|use`.
pub fn run(a: &Args, env: Env) -> Result<Value, String> {
    let action = a.pos.first().map(String::as_str).unwrap_or("list");
    let name = a.pos.get(1).map(String::as_str);
    let need = || name.ok_or(format!("pgbx profile {action} <name>"));
    let mut st = load(env)?;
    let file = path(env)?.display().to_string();
    match action {
        "list" => {
            let ps: Vec<Value> = st.profiles.keys().map(|n| show(n, &st)).collect();
            Ok(json!({"ok": true, "file": file, "default": st.default, "profiles": ps}))
        }
        "show" => {
            let n = need()?;
            if !st.profiles.contains_key(n) {
                return Err(format!("no profile '{n}' (pgbx profile list)"));
            }
            Ok(json!({"ok": true, "file": file, "profile": show(n, &st)}))
        }
        "add" => {
            let n = need()?;
            check_name(n)?;
            let mut m = Map::new();
            for (k, _) in KEYS {
                if let Some(v) = a.flags.get(*k) {
                    m.insert(k.to_string(), json!(v));
                }
            }
            if let Some(p) = m.get("port").and_then(|v| v.as_str()) {
                p.parse::<u16>().map_err(|_| format!("bad --port '{p}'"))?;
            }
            if m.is_empty() {
                return Err(format!("pgbx profile add {n} needs at least one of: {}",
                    KEYS.iter().map(|(k, _)| format!("--{k}")).collect::<Vec<_>>().join(" ")));
            }
            let replaced = st.profiles.insert(n.to_string(), Value::Object(m)).is_some();
            if st.default.is_none() {
                st.default = Some(n.to_string()); // the first profile becomes the default
            }
            save(env, &st)?;
            Ok(json!({"ok": true, "file": file, "replaced": replaced, "profile": show(n, &st),
                "note": "no passwords or S3 keys are stored: use ~/.pgpass or PGPASSWORD, and --credentials-file for S3"}))
        }
        "remove" => {
            let n = need()?;
            if st.profiles.remove(n).is_none() {
                return Err(format!("no profile '{n}' (pgbx profile list)"));
            }
            if st.default.as_deref() == Some(n) {
                st.default = None;
            }
            save(env, &st)?;
            Ok(json!({"ok": true, "file": file, "removed": n, "default": st.default}))
        }
        "use" => {
            let n = need()?;
            if !st.profiles.contains_key(n) {
                return Err(format!("no profile '{n}' (pgbx profile list)"));
            }
            st.default = Some(n.to_string());
            save(env, &st)?;
            Ok(json!({"ok": true, "file": file, "default": n}))
        }
        x => Err(format!("unknown profile action '{x}' (add | list | show | remove | use)")),
    }
}

pub fn run_sys(a: &Args) -> Result<Value, String> {
    run(a, &sys_env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_args;

    fn p(s: &[&str]) -> Args {
        parse_args(s.iter().map(|x| x.to_string())).unwrap()
    }
    fn store() -> Store {
        Store::from_json(r#"{"default":"dev","profiles":{
            "dev":{"host":"localhost","port":"5433","user":"app"},
            "prod":{"host":"db.prod","user":"ops","admin-db":"admin","s3-bucket":"b"}}}"#).unwrap()
    }
    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn default_profile_fills_missing_flags() {
        let mut a = p(&["status"]);
        assert_eq!(apply(&mut a, &store(), &no_env).unwrap().as_deref(), Some("dev"));
        assert_eq!(a.flags.get("host").map(String::as_str), Some("localhost"));
        assert_eq!(a.flags.get("port").map(String::as_str), Some("5433"));
    }

    #[test]
    fn precedence_flag_env_profile() {
        let env = |k: &str| (k == "PGUSER").then(|| "envuser".to_string());
        let mut a = p(&["status", "--profile", "prod", "--host", "flaghost"]);
        assert_eq!(apply(&mut a, &store(), &env).unwrap().as_deref(), Some("prod"));
        assert_eq!(a.flags["host"], "flaghost"); // flag beats profile
        assert!(!a.flags.contains_key("user")); // PGUSER beats profile: left for connect() to read
        assert_eq!(a.flags["admin-db"], "admin");
        assert_eq!(a.flags["s3-bucket"], "b");
    }

    #[test]
    fn env_picks_profile_and_flag_beats_env() {
        let env = |k: &str| (k == "PGBX_PROFILE").then(|| "prod".to_string());
        let mut a = p(&["status"]);
        assert_eq!(apply(&mut a, &store(), &env).unwrap().as_deref(), Some("prod"));
        let mut a = p(&["status", "--profile", "dev"]);
        assert_eq!(apply(&mut a, &store(), &env).unwrap().as_deref(), Some("dev"));
    }

    #[test]
    fn no_profile_keeps_old_behaviour() {
        let mut a = p(&["status", "--db", "x"]);
        assert_eq!(apply(&mut a, &Store::default(), &no_env).unwrap(), None);
        assert_eq!(a, p(&["status", "--db", "x"]));
        let mut a = p(&["status", "--profile", "nope"]);
        assert!(apply(&mut a, &store(), &no_env).unwrap_err().contains("no profile 'nope'"));
    }

    #[test]
    fn crud_round_trip_and_mode() {
        let dir = std::env::temp_dir().join(format!("pgbx-prof-{}", std::process::id()));
        let d = dir.display().to_string();
        let env = move |k: &str| (k == "PGBX_CONFIG_DIR").then(|| d.clone());
        let r = run(&p(&["profile", "add", "prod", "--host", "h", "--port", "6432", "--s3-bucket", "b"]), &env).unwrap();
        assert_eq!(r["profile"]["default"], true);
        run(&p(&["profile", "add", "dev", "--host", "localhost"]), &env).unwrap();
        assert!(run(&p(&["profile", "add", "x", "--port", "nan"]), &env).is_err());
        assert!(run(&p(&["profile", "add", "bad/name", "--host", "h"]), &env).is_err());
        assert!(run(&p(&["profile", "add", "empty"]), &env).is_err());
        let l = run(&p(&["profile", "list"]), &env).unwrap();
        assert_eq!(l["profiles"].as_array().unwrap().len(), 2);
        assert_eq!(l["default"], "prod");
        run(&p(&["profile", "use", "dev"]), &env).unwrap();
        assert_eq!(run(&p(&["profile", "show", "dev"]), &env).unwrap()["profile"]["default"], true);
        assert!(run(&p(&["profile", "use", "nope"]), &env).is_err());
        let txt = std::fs::read_to_string(dir.join("profiles.json")).unwrap();
        assert!(!txt.contains("password") && !txt.contains("secret"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let m = std::fs::metadata(dir.join("profiles.json")).unwrap().permissions().mode();
            assert_eq!(m & 0o777, 0o600);
        }
        let r = run(&p(&["profile", "remove", "dev"]), &env).unwrap();
        assert_eq!(r["default"], Value::Null);
        assert!(run(&p(&["profile", "remove", "dev"]), &env).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_dir_resolution() {
        let x = |k: &str| match k { "XDG_CONFIG_HOME" => Some("/x".into()), "HOME" => Some("/h".into()), _ => None };
        let h = |k: &str| (k == "HOME").then(|| "/h".to_string());
        if !cfg!(windows) {
            assert_eq!(config_dir(&x).unwrap(), PathBuf::from("/x/pgbx"));
            assert_eq!(config_dir(&h).unwrap(), PathBuf::from("/h/.config/pgbx"));
        }
        assert!(config_dir(&no_env).is_err());
    }
}
