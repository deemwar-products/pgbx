//! Named connection profiles (ADR 0003), stored in config.yaml (see config.rs). A profile is either
//!   url: postgres://$PGUSER:$PGPASSWORD@host:5432/db       a connection string, or
//!   adapter: NAME  + free-form keys for that adapter      an external command that hands pgbx a connection string
//! plus pgbx's own keys (admin-db, s3-endpoint, s3-bucket, s3-region, server-name, credentials-file, ready_timeout).
//! Profiles hold `$VAR` references, never values: a literal password is refused.
//!
//! Which connection a command uses: --url > --profile > --host/--port flags (direct) > PGBX_URL > PGBX_PROFILE >
//! the default profile (pgbx profile use) > direct from PGHOST/PGPORT/PGUSER and built-in defaults.

use crate::config::{self, Config, Env};
use crate::conn::{self, Conn, Spec};
use crate::vars::{self, Resolver, Source};
use crate::Args;
use serde_json::{json, Map, Value};
use std::sync::Arc;
use std::time::Duration;

/// pgbx's own profile keys; everything else in an adapter profile is that adapter's config.
pub const PGBX_KEYS: &[&str] = &["adapter", "url", "ready_timeout", "admin-db", "s3-endpoint", "s3-bucket", "s3-region", "server-name", "credentials-file"];
/// Profile keys that fill a command's flags (an explicit flag wins).
pub const FLAG_KEYS: &[&str] = &["admin-db", "s3-endpoint", "s3-bucket", "s3-region", "server-name", "credentials-file"];

pub fn check_name(n: &str) -> Result<(), String> {
    if !n.is_empty() && n.len() <= 64 && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.') {
        Ok(())
    } else {
        Err(format!("bad profile name '{n}': use letters, digits, '-', '_' or '.'"))
    }
}

/// A key that holds a secret: its value must be a `$VAR` reference.
fn secret_key(k: &str) -> bool {
    let l = k.to_ascii_lowercase();
    ["password", "passwd", "secret", "token"].iter().any(|w| l.contains(w))
}

/// The password part of a connection string, if it has one (URL userinfo or password=...).
fn url_password(u: &str) -> Option<String> {
    if let Some((_, rest)) = u.split_once("://") {
        let auth = &rest[..rest.find(['/', '?']).unwrap_or(rest.len())];
        let ui = auth.rsplit_once('@')?.0;
        return ui.split_once(':').map(|(_, p)| p.to_string()).filter(|p| !p.is_empty());
    }
    u.split_whitespace().find_map(|kv| kv.strip_prefix("password=")).map(|p| p.trim_matches('\'').to_string())
}

/// Refuse to write a secret: url passwords and secret-looking keys must be `$VAR` references.
pub fn check_no_literal_secrets(p: &Map<String, Value>) -> Result<(), String> {
    if let Some(u) = p.get("url").and_then(|v| v.as_str()) {
        if url_password(u).is_some_and(|pw| !vars::is_reference(&pw)) {
            return Err("refusing to store a password: write a reference instead, e.g. --url 'postgres://user:$PGPASSWORD@host:5432/db' \
                        (single quotes, so your shell leaves $PGPASSWORD alone); pgbx expands it at run time".into());
        }
    }
    for (k, v) in p {
        if secret_key(k) && v.as_str().is_some_and(vars::is_literal_secret) {
            return Err(format!("refusing to store a secret in '{k}': use a reference like {k}=$MY_SECRET (expanded at run time from the \
                                environment or your secrets: source)"));
        }
    }
    Ok(())
}

/// The profile with secrets masked, for show/list.
pub fn redacted(p: &Value) -> Value {
    let mut m = p.as_object().cloned().unwrap_or_default();
    for (k, v) in m.iter_mut() {
        if let Some(s) = v.as_str() {
            if k == "url" {
                *v = json!(conn::redact(s));
            } else if secret_key(k) && vars::is_literal_secret(s) {
                *v = json!("***");
            }
        }
    }
    Value::Object(m)
}

fn ready_timeout(p: &Value) -> Result<Duration, String> {
    match p.get("ready_timeout") {
        None | Some(Value::Null) => Ok(crate::adapter::READY_TIMEOUT),
        Some(Value::Number(n)) => Ok(Duration::from_secs(n.as_u64().unwrap_or(30))),
        Some(Value::String(s)) => crate::query::parse_duration(s).map_err(|e| e.replace("--timeout", "ready_timeout")),
        Some(_) => Err("ready_timeout: a duration like 30s".into()),
    }
}

/// The connection of profile `name` in `cfg` (nothing is started yet).
pub fn conn_of(name: &str, cfg: &Config, env: Env) -> Result<Conn, String> {
    let p = cfg.profiles.get(name).ok_or_else(|| format!("no profile '{name}' (pgbx profile list)"))?;
    let dir = config::config_dir(env)?;
    let source = Source::parse(cfg.secrets.as_ref(), &dir)?;
    let spec = match (p.get("url").and_then(|v| v.as_str()), p.get("adapter").and_then(|v| v.as_str())) {
        (Some(_), Some(_)) => return Err(format!("profile '{name}' has both url and adapter: keep one")),
        (Some(u), None) => Spec::Url { profile: Some(name.into()), raw: u.into() },
        (None, Some(a)) => {
            let config: Map<String, Value> = p.as_object().unwrap().iter()
                .filter(|(k, _)| !PGBX_KEYS.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
            Spec::Adapter { profile: name.into(), adapter: a.into(), argv: cfg.adapter_argv(a, &dir)?, config: Value::Object(config),
                            ready_timeout: ready_timeout(p)? }
        }
        (None, None) => return Err(format!("profile '{name}' has neither url nor adapter (pgbx profile edit {name} --url ... | --adapter ...)")),
    };
    let mut c = Conn::new(spec, source, dir);
    // sslmode / sslrootcert: pgbx reads them from any profile; an adapter also gets them in its config (they are not
    // PGBX_KEYS), so the adapter can put them in its URL, which then wins
    let s = |k: &str| p.get(k).and_then(|v| v.as_str()).map(String::from);
    c.ssl = crate::tls::Params { mode: s("sslmode"), rootcert: s("sslrootcert") };
    Ok(c)
}

/// Fill a command's flags from the profile's pgbx keys ($VARs expanded) where no flag is given.
fn fill_flags(a: &mut Args, p: &Value, cfg: &Config, env: Env) -> Result<(), String> {
    let dir = config::config_dir(env)?;
    let mut r = Resolver::new(env, Source::parse(cfg.secrets.as_ref(), &dir)?, dir);
    for k in FLAG_KEYS {
        if a.flags.contains_key(*k) {
            continue;
        }
        if let Some(v) = p.get(*k).and_then(|v| v.as_str()) {
            a.flags.insert(k.to_string(), r.expand(v)?);
        }
    }
    Ok(())
}

/// Pick the connection for a command (see the module doc). Returns it and any one-time notices (migration).
pub fn select(a: &mut Args, env: Env) -> Result<(Arc<Conn>, Vec<String>), String> {
    let flag_url = a.flags.get("url").cloned();
    let flag_profile = a.flags.get("profile").cloned();
    if flag_url.is_some() && flag_profile.is_some() {
        return Err("give --url or --profile, not both".into());
    }
    let direct_flags = a.flags.contains_key("host") || a.flags.contains_key("port");
    let direct = |a: &Args| Arc::new(Conn::direct(a.flags.get("host").map(String::as_str), a.flags.get("port").map(String::as_str),
                                                  a.flags.get("user").map(String::as_str)));
    let url_conn = |raw: String, env: Env, cfg: Option<&Config>| -> Result<Arc<Conn>, String> {
        let dir = config::config_dir(env).unwrap_or_default();
        let src = match cfg {
            Some(c) => Source::parse(c.secrets.as_ref(), &dir)?,
            None => Source::Env,
        };
        let mut c = Conn::new(Spec::Url { profile: None, raw }, src, dir);
        c.user = a.flags.get("user").cloned();
        Ok(Arc::new(c))
    };
    let wants_profile = flag_profile.is_some() || (!direct_flags && flag_url.is_none() && env("PGBX_URL").is_none() && env("PGBX_PROFILE").is_some());
    let loaded = config::load(env);
    let (cfg, notes) = match loaded {
        Ok(x) => x,
        Err(e) if wants_profile || flag_url.is_some() => return Err(e),
        Err(_) => (Config::default(), vec![]),
    };
    if let Some(u) = flag_url {
        return Ok((url_conn(u, env, Some(&cfg))?, notes));
    }
    let name = match flag_profile {
        Some(p) => Some(p),
        None if direct_flags => return Ok((direct(a), notes)),
        None => {
            if let Some(u) = env("PGBX_URL") {
                return Ok((url_conn(u, env, Some(&cfg))?, notes));
            }
            env("PGBX_PROFILE").or(cfg.default.clone())
        }
    };
    let Some(name) = name else { return Ok((direct(a), notes)) };
    let Some(p) = cfg.profiles.get(&name) else {
        let explicit = a.flags.contains_key("profile") || env("PGBX_PROFILE").is_some();
        return Err(if explicit {
            format!("no profile '{name}' (pgbx profile list)")
        } else {
            format!("default profile '{name}' no longer exists (pgbx profile use <name>)")
        });
    };
    fill_flags(a, p, &cfg, env)?;
    a.flags.insert("profile".into(), name.clone());
    let mut c = conn_of(&name, &cfg, env)?;
    c.user = a.flags.get("user").cloned();
    Ok((Arc::new(c), notes))
}

/// main's entry point.
pub fn select_sys(a: &mut Args) -> Result<(Arc<Conn>, Vec<String>), String> {
    select(a, &config::sys_env)
}

/// The config on disk (`pgbx serve` lists it for its connection switcher). Notices are printed once.
pub fn load_sys() -> Result<Config, String> {
    let (c, notes) = config::load(&config::sys_env)?;
    for n in notes {
        eprintln!("pgbx: {n}");
    }
    Ok(c)
}

/// Args and connection for one named profile (`pgbx serve`'s switcher).
pub fn args_for(name: &str, cfg: &Config) -> Result<(Args, Conn), String> {
    let mut a = Args { cmd: "serve".into(), ..Default::default() };
    let p = cfg.profiles.get(name).ok_or_else(|| format!("no profile '{name}' (pgbx profile list)"))?;
    fill_flags(&mut a, p, cfg, &config::sys_env)?;
    a.flags.insert("profile".into(), name.to_string());
    Ok((a, conn_of(name, cfg, &config::sys_env)?))
}

fn runs(cfg: &Config, p: &Value, env: Env) -> Value {
    match p.get("adapter").and_then(|v| v.as_str()) {
        Some(a) => match config::config_dir(env).and_then(|d| cfg.adapter_argv(a, &d)) {
            Ok(v) => json!(v.join(" ")),
            Err(e) => json!(format!("(not runnable: {e})")),
        },
        None => Value::Null,
    }
}

fn show(name: &str, cfg: &Config, env: Env) -> Value {
    let p = &cfg.profiles[name];
    let kind = if p.get("adapter").is_some() { "adapter" } else if p.get("url").is_some() { "url" } else { "incomplete" };
    json!({"name": name, "default": cfg.default.as_deref() == Some(name), "connection": kind,
           "adapter": p.get("adapter"), "runs": runs(cfg, p, env), "settings": redacted(p)})
}

/// Apply flags and key=value arguments to a profile map. `key=` removes a key.
fn apply_changes(m: &mut Map<String, Value>, a: &Args, kv: &[String]) -> Result<(), String> {
    if let Some(u) = a.flags.get("url") {
        m.retain(|k, _| PGBX_KEYS.contains(&k.as_str()) && k != "adapter");
        m.insert("url".into(), json!(u));
    }
    if a.flags.contains_key("user") && !a.flags.contains_key("host") && !a.flags.contains_key("port") {
        return Err("--user goes with --host/--port (a url profile); in a url write it in the url, for an adapter use user=NAME".into());
    }
    if a.flags.contains_key("host") || a.flags.contains_key("port") {
        if a.flags.contains_key("url") {
            return Err("give --url or --host/--port/--user, not both".into());
        }
        let host = a.flags.get("host").cloned().unwrap_or_else(|| "localhost".into());
        let host = if host.starts_with('/') { vars::pct(&host) } else { host };
        let user = a.flags.get("user").map(|u| format!("{}@", vars::pct(u))).unwrap_or_default();
        let port = match a.flags.get("port") {
            Some(p) => {
                p.parse::<u16>().map_err(|_| format!("bad --port '{p}'"))?;
                format!(":{p}")
            }
            None => String::new(),
        };
        m.retain(|k, _| PGBX_KEYS.contains(&k.as_str()) && k != "adapter");
        m.insert("url".into(), json!(format!("postgres://{user}{host}{port}/")));
    }
    if let Some(ad) = a.flags.get("adapter") {
        if a.flags.contains_key("url") {
            return Err("give --url or --adapter, not both".into());
        }
        m.remove("url");
        m.insert("adapter".into(), json!(ad));
    }
    for k in FLAG_KEYS {
        if let Some(v) = a.flags.get(*k) {
            m.insert(k.to_string(), json!(v));
        }
    }
    for item in kv {
        let (k, v) = item.split_once('=').ok_or_else(|| format!("'{item}': adapter settings are key=value (key= removes one)"))?;
        let k = k.trim();
        if k.is_empty() || k == "adapter" || k == "url" {
            return Err(format!("'{item}': use --adapter / --url for those"));
        }
        if v.is_empty() {
            m.remove(k);
        } else {
            m.insert(k.to_string(), json!(v));
        }
    }
    Ok(())
}

/// Make sure the profile's adapter is defined: --adapter-command defines it; a shipped example is registered.
fn ensure_adapter(cfg: &mut Config, a: &Args, m: &Map<String, Value>, env: Env, notes: &mut Vec<String>) -> Result<(), String> {
    let Some(name) = m.get("adapter").and_then(|v| v.as_str()) else {
        if a.flags.contains_key("adapter-command") {
            return Err("--adapter-command needs --adapter NAME".into());
        }
        return Ok(());
    };
    check_name(name).map_err(|e| e.replace("profile name", "adapter name"))?;
    if let Some(c) = a.flags.get("adapter-command") {
        let argv = config::argv(&json!(c), &config::config_dir(env)?)?;
        cfg.adapters.insert(name.to_string(), json!(c));
        notes.push(format!("adapter '{name}' runs: {} (it gets the profile's settings on stdin and runs with your rights)", argv.join(" ")));
        return Ok(());
    }
    if cfg.adapters.contains_key(name) {
        return Ok(());
    }
    let js = config::adapters_dir(env)?.join(name).join(format!("{name}-adapter.js"));
    if js.is_file() {
        let c = config::example_command(env, name)?;
        notes.push(format!("adapter '{name}' registered: {} (the shipped example; needs Node 18+)", match &c {
            Value::String(s) => s.clone(),
            x => x.to_string(),
        }));
        cfg.adapters.insert(name.to_string(), c);
        return Ok(());
    }
    Err(format!("no adapter '{name}' in {}: add --adapter-command '<command>' (for the shipped examples: \
                 node /path/to/pgbx/adapters/{name}/{name}-adapter.js), or copy the pgbx adapters/ folder to {}",
        config::FILE, config::adapters_dir(env)?.display()))
}

/// `pgbx profile add|edit|remove|list|show|use`.
pub fn run(a: &Args, env: Env) -> Result<Value, String> {
    let action = a.pos.first().map(String::as_str).unwrap_or("list");
    let name = a.pos.get(1).map(String::as_str);
    let need = || name.ok_or(format!("pgbx profile {action} <name>"));
    let (mut cfg, mut notes) = config::load(env)?;
    let file = config::path(env)?.display().to_string();
    let out = match action {
        "list" => {
            let ps: Vec<Value> = cfg.profiles.keys().map(|n| show(n, &cfg, env)).collect();
            json!({"ok": true, "file": file, "default": cfg.default, "profiles": ps,
                   "adapters": cfg.adapters, "secrets": cfg.secrets.clone().unwrap_or(json!("env"))})
        }
        "show" => {
            let n = need()?;
            if !cfg.profiles.contains_key(n) {
                return Err(format!("no profile '{n}' (pgbx profile list)"));
            }
            json!({"ok": true, "file": file, "profile": show(n, &cfg, env)})
        }
        "add" | "edit" => {
            let n = need()?;
            check_name(n)?;
            let exists = cfg.profiles.contains_key(n);
            if action == "edit" && !exists {
                return Err(format!("no profile '{n}' (pgbx profile add {n} ...)"));
            }
            let mut m = if action == "edit" { cfg.profiles[n].as_object().cloned().unwrap_or_default() } else { Map::new() };
            apply_changes(&mut m, a, &a.pos[2..])?;
            if !m.contains_key("url") && !m.contains_key("adapter") {
                return Err(format!("pgbx profile {action} {n} needs --url 'postgres://user:$PGPASSWORD@host:5432/db' or --adapter NAME [key=value ...]"));
            }
            if m.contains_key("url") {
                if let Some(extra) = m.keys().find(|k| !PGBX_KEYS.contains(&k.as_str())) {
                    return Err(format!("'{extra}' is an adapter setting, but profile {n} is a url profile"));
                }
            }
            check_no_literal_secrets(&m)?;
            ready_timeout(&Value::Object(m.clone()))?;
            ensure_adapter(&mut cfg, a, &m, env, &mut notes)?;
            cfg.profiles.insert(n.to_string(), Value::Object(m));
            if cfg.default.is_none() {
                cfg.default = Some(n.to_string()); // the first profile becomes the default
            }
            config::save(env, &cfg)?;
            json!({"ok": true, "file": file, "replaced": action == "add" && exists, "profile": show(n, &cfg, env),
                   "note": "no secrets are stored: profiles hold $VAR references, expanded at run time from the environment or your secrets: source"})
        }
        "remove" => {
            let n = need()?;
            if cfg.profiles.remove(n).is_none() {
                return Err(format!("no profile '{n}' (pgbx profile list)"));
            }
            if cfg.default.as_deref() == Some(n) {
                cfg.default = None;
            }
            config::save(env, &cfg)?;
            json!({"ok": true, "file": file, "removed": n, "default": cfg.default})
        }
        "use" => {
            let n = need()?;
            if !cfg.profiles.contains_key(n) {
                return Err(format!("no profile '{n}' (pgbx profile list)"));
            }
            cfg.default = Some(n.to_string());
            config::save(env, &cfg)?;
            json!({"ok": true, "file": file, "default": n})
        }
        x => return Err(format!("unknown profile action '{x}' (add | edit | list | show | remove | use)")),
    };
    let mut out = out;
    if !notes.is_empty() {
        out["notices"] = json!(notes);
    }
    Ok(out)
}

pub fn run_sys(a: &Args) -> Result<Value, String> {
    run(a, &config::sys_env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_args;
    use std::path::PathBuf;

    fn p(s: &[&str]) -> Args {
        parse_args(s.iter().map(|x| x.to_string())).unwrap()
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pgbx-prof-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn env_in(d: &std::path::Path, extra: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        let ds = d.display().to_string();
        move |k: &str| {
            if k == "PGBX_CONFIG_DIR" {
                return Some(ds.clone());
            }
            extra.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn crud_round_trip_mode_and_no_secrets_on_disk() {
        let d = tmp("crud");
        let env = env_in(&d, &[]);
        let r = run(&p(&["profile", "add", "dev", "--url", "postgres://app:$PGPASSWORD@localhost:5433/shop", "--s3-bucket", "b"]), &env).unwrap();
        assert_eq!((r["profile"]["default"].clone(), r["profile"]["connection"].as_str()), (json!(true), Some("url")));
        let e = run(&p(&["profile", "add", "leak", "--url", "postgres://app:Hunter2-Literal@h/db"]), &env).unwrap_err();
        assert!(e.contains("refusing to store a password") && !e.contains("Hunter2"), "{e}");
        let e = run(&p(&["profile", "add", "leak", "--adapter-command", "x", "--adapter", "corp", "password=Hunter2-Literal"]), &env).unwrap_err();
        assert!(e.contains("refusing to store a secret in 'password'"), "{e}");
        let r = run(&p(&["profile", "add", "prod", "--adapter", "corp", "--adapter-command", "/usr/local/bin/corp-pg --fast",
                         "account=acme-prod", "region=eu-west-1", "password=$CORP_PW", "--s3-bucket", "bk"]), &env).unwrap();
        assert_eq!(r["profile"]["runs"], "/usr/local/bin/corp-pg --fast");
        assert!(r["notices"][0].as_str().unwrap().contains("runs: /usr/local/bin/corp-pg --fast"));
        assert_eq!(r["profile"]["settings"]["account"], "acme-prod");
        // edit: change a key, remove one, switch to a url and back
        run(&p(&["profile", "edit", "prod", "region=us-east-1", "account="]), &env).unwrap();
        let s = run(&p(&["profile", "show", "prod"]), &env).unwrap();
        assert_eq!(s["profile"]["settings"]["region"], "us-east-1");
        assert!(s["profile"]["settings"].get("account").is_none());
        assert_eq!(s["profile"]["settings"]["s3-bucket"], "bk");
        assert!(run(&p(&["profile", "edit", "nope", "a=b"]), &env).is_err());
        assert!(run(&p(&["profile", "edit", "dev", "target=x"]), &env).unwrap_err().contains("adapter setting"));
        assert!(run(&p(&["profile", "add", "bad/name", "--url", "postgres://h/d"]), &env).is_err());
        assert!(run(&p(&["profile", "add", "empty"]), &env).unwrap_err().contains("--url"));
        assert!(run(&p(&["profile", "add", "x", "--adapter", "nosuch"]), &env).unwrap_err().contains("no adapter 'nosuch'"));
        assert!(run(&p(&["profile", "add", "x", "--host", "h", "--port", "nan"]), &env).is_err());
        let r = run(&p(&["profile", "add", "direct", "--host", "db1", "--port", "6432", "--user", "ops"]), &env).unwrap();
        assert_eq!(r["profile"]["settings"]["url"], "postgres://ops@db1:6432/");
        let l = run(&p(&["profile", "list"]), &env).unwrap();
        assert_eq!(l["profiles"].as_array().unwrap().len(), 3);
        assert_eq!((l["default"].as_str(), l["secrets"].as_str()), (Some("dev"), Some("env")));
        run(&p(&["profile", "use", "prod"]), &env).unwrap();
        assert_eq!(run(&p(&["profile", "show", "prod"]), &env).unwrap()["profile"]["default"], true);
        assert!(run(&p(&["profile", "use", "nope"]), &env).is_err());
        let txt = std::fs::read_to_string(d.join("config.yaml")).unwrap();
        assert!(txt.contains("$PGPASSWORD") && txt.contains("$CORP_PW") && !txt.contains("Hunter2"), "{txt}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(d.join("config.yaml")).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let r = run(&p(&["profile", "remove", "prod"]), &env).unwrap();
        assert_eq!(r["default"], Value::Null);
        assert!(run(&p(&["profile", "remove", "prod"]), &env).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn show_redacts_url_passwords() {
        let d = tmp("redact");
        std::fs::create_dir_all(&d).unwrap();
        // hand-edited file with a literal password: shown masked, never echoed
        std::fs::write(d.join("config.yaml"), "profiles:\n  x:\n    url: postgres://u:Literal-Pw-5@h/d\n  y:\n    adapter: a\n    token: tok-literal-55\n").unwrap();
        let env = env_in(&d, &[]);
        let s = run(&p(&["profile", "show", "x"]), &env).unwrap().to_string();
        assert!(s.contains("postgres://u:***@h/d") && !s.contains("Literal-Pw-5"), "{s}");
        let s = run(&p(&["profile", "list"]), &env).unwrap().to_string();
        assert!(!s.contains("Literal-Pw-5") && !s.contains("tok-literal-55"), "{s}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn selection_order() {
        let d = tmp("sel");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("config.yaml"), "default: dev\nadapters:\n  t: sh t.sh\nprofiles:\n  dev:\n    url: postgres://a@devhost/x\n    admin-db: adm\n\
              \x20 prod:\n    adapter: t\n    target: $TGT\n    s3-bucket: $BUCKET\n    ready_timeout: 3s\n").unwrap();
        let env = env_in(&d, &[("BUCKET", "bk1")]);
        let mut a = p(&["status"]);
        let (c, _) = select(&mut a, &env).unwrap();
        assert_eq!((c.profile(), a.flags["admin-db"].as_str()), (Some("dev"), "adm"));
        let mut a = p(&["status", "--profile", "prod"]);
        let (c, _) = select(&mut a, &env).unwrap();
        assert!(c.is_adapter());
        assert_eq!(a.flags["s3-bucket"], "bk1", "pgbx keys are $VAR-expanded into flags");
        match &c.spec {
            Spec::Adapter { config, ready_timeout, argv, .. } => {
                assert_eq!(config, &json!({"target": "$TGT"}), "adapter config only, expanded when it starts");
                assert_eq!(*ready_timeout, Duration::from_secs(3));
                assert_eq!(argv, &["sh", "t.sh"]);
            }
            x => panic!("{x:?}"),
        }
        let mut a = p(&["status", "--profile", "dev", "--user", "viewer"]);
        assert_eq!(select(&mut a, &env).unwrap().0.user.as_deref(), Some("viewer"), "--user beats the url's user");
        let mut a = p(&["status", "--host", "h2"]);
        assert!(select(&mut a, &env).unwrap().0.profile().is_none(), "--host means direct, not the default profile");
        let mut a = p(&["status", "--url", "postgres://u@h/d"]);
        assert!(matches!(select(&mut a, &env).unwrap().0.spec, Spec::Url { profile: None, .. }));
        assert!(select(&mut p(&["status", "--url", "x", "--profile", "dev"]), &env).unwrap_err().contains("not both"));
        assert!(select(&mut p(&["status", "--profile", "nope"]), &env).unwrap_err().contains("no profile 'nope'"));
        let env2 = env_in(&d, &[("PGBX_URL", "postgres://u@envhost/d"), ("PGBX_PROFILE", "prod")]);
        let mut a = p(&["status"]);
        assert!(matches!(select(&mut a, &env2).unwrap().0.spec, Spec::Url { profile: None, .. }), "PGBX_URL beats PGBX_PROFILE");
        let mut a = p(&["status", "--profile", "dev"]);
        assert_eq!(select(&mut a, &env2).unwrap().0.profile(), Some("dev"), "--profile beats PGBX_URL");
        let none = tmp("sel-none");
        let mut a = p(&["status"]);
        assert!(select(&mut a, &env_in(&none, &[])).unwrap().0.profile().is_none());
        let _ = std::fs::remove_dir_all(&d);
    }
}
