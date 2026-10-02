//! pgbx's one config file (ADR 0003): `<config dir>/config.yaml`, mode 0600 (it names commands pgbx runs, so it
//! is as sensitive as ~/.ssh/config). Config dir: $PGBX_CONFIG_DIR, else %APPDATA%\pgbx on Windows, else
//! $XDG_CONFIG_HOME/pgbx, else ~/.config/pgbx.
//!
//! ```yaml
//! default: prod                 # the profile used when none is named (pgbx profile use)
//! secrets: env                  # where $VARs come from after the environment: env | a .env path | a command
//! adapters:                     # name -> command (a string split like a shell would, but run without one; or a list)
//!   ssh: node ~/.config/pgbx/adapters/ssh/ssh-adapter.js
//! profiles:
//!   prod:                       # an adapter + whatever config that adapter reads (free-form, $VARs allowed)
//!     adapter: ssh
//!     target: ops@db1
//!     user: app
//!     password: $PGPASSWORD
//!   dev:                        # or a plain connection string
//!     url: postgres://$PGUSER:$PGPASSWORD@localhost:5432/shop
//! ```
//! An older `profiles.json` is migrated on first use: ssh profiles become `adapter: ssh` profiles (with an
//! `adapters: ssh:` entry pointing at the shipped example), direct ones become `url:` profiles. The old file is
//! kept as `profiles.json.migrated`.

use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

pub const FILE: &str = "config.yaml";

pub fn sys_env(k: &str) -> Option<String> {
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

/// Where the example adapters live on this machine (install.sh puts them here): $PGBX_ADAPTERS_DIR or <config dir>/adapters.
pub fn adapters_dir(env: Env) -> Result<PathBuf, String> {
    match env("PGBX_ADAPTERS_DIR") {
        Some(d) => Ok(PathBuf::from(d)),
        None => Ok(config_dir(env)?.join("adapters")),
    }
}

/// The command that runs a shipped example adapter (`node <dir>/<name>/<name>-adapter.js`).
pub fn example_command(env: Env, name: &str) -> Result<Value, String> {
    let js = adapters_dir(env)?.join(name).join(format!("{name}-adapter.js"));
    let js = js.display().to_string();
    Ok(if js.contains(char::is_whitespace) { json!(["node", js]) } else { json!(format!("node {js}")) })
}

/// `~/...` -> $HOME/... (commands run without a shell, so nobody else expands it).
pub fn tilde(s: &str) -> String {
    match s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) {
        Some(r) => match sys_env("HOME").or_else(|| sys_env("USERPROFILE")) {
            Some(h) => Path::new(&h).join(r).display().to_string(),
            None => s.to_string(),
        },
        None => s.to_string(),
    }
}

/// Split a command line into argv like a POSIX shell would for words and quotes (no variables, globs or pipes).
pub fn split_words(s: &str) -> Result<Vec<String>, String> {
    let mut v = vec![];
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(x) => cur.push(x),
                        None => return Err(format!("unterminated ' in command: {s}")),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(x @ ('"' | '\\')) => cur.push(x),
                            Some(x) => {
                                cur.push('\\');
                                cur.push(x);
                            }
                            None => return Err(format!("unterminated \" in command: {s}")),
                        },
                        Some(x) => cur.push(x),
                        None => return Err(format!("unterminated \" in command: {s}")),
                    }
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    v.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        v.push(cur);
    }
    Ok(v)
}

/// A command from the config: a string (split into words) or a list of strings. `~/` is expanded.
pub fn argv(v: &Value, _dir: &Path) -> Result<Vec<String>, String> {
    let words = match v {
        Value::String(s) => split_words(s)?,
        Value::Array(a) => a.iter().map(|x| x.as_str().map(String::from).ok_or("a command list holds strings only")).collect::<Result<_, _>>()?,
        _ => return Err("a command is a string or a list of strings".into()),
    };
    if words.is_empty() {
        return Err("empty command".into());
    }
    Ok(words.iter().map(|w| tilde(w)).collect())
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Config {
    pub default: Option<String>,
    pub secrets: Option<Value>,
    pub adapters: Map<String, Value>,
    pub profiles: Map<String, Value>,
}

impl Config {
    pub fn from_yaml(s: &str) -> Result<Config, String> {
        if s.trim().is_empty() {
            return Ok(Config::default());
        }
        let v: Value = serde_norway::from_str(s).map_err(|e| format!("not valid YAML: {e}"))?;
        let v = match v {
            Value::Null => return Ok(Config::default()),
            Value::Object(m) => m,
            _ => return Err("the top level must be a mapping (default, secrets, adapters, profiles)".into()),
        };
        for k in v.keys() {
            if !["default", "secrets", "adapters", "profiles"].contains(&k.as_str()) {
                return Err(format!("unknown top-level key '{k}' (default, secrets, adapters, profiles)"));
            }
        }
        let obj = |k: &str| -> Result<Map<String, Value>, String> {
            match v.get(k) {
                None | Some(Value::Null) => Ok(Map::new()),
                Some(Value::Object(m)) => Ok(m.clone()),
                Some(_) => Err(format!("'{k}' must be a mapping of name -> ...")),
            }
        };
        let profiles = obj("profiles")?;
        for (n, p) in &profiles {
            if !p.is_object() {
                return Err(format!("profile '{n}' must be a mapping (url: ... or adapter: ...)"));
            }
        }
        Ok(Config {
            default: v.get("default").and_then(|d| d.as_str()).map(String::from),
            secrets: v.get("secrets").cloned().filter(|s| !s.is_null() && s.as_str() != Some("env")),
            adapters: obj("adapters")?,
            profiles,
        })
    }

    pub fn to_yaml(&self) -> String {
        let mut m = Map::new();
        if let Some(d) = &self.default {
            m.insert("default".into(), json!(d));
        }
        m.insert("secrets".into(), self.secrets.clone().unwrap_or(json!("env")));
        m.insert("adapters".into(), Value::Object(self.adapters.clone()));
        m.insert("profiles".into(), Value::Object(self.profiles.clone()));
        let body = serde_norway::to_string(&Value::Object(m)).unwrap_or_default();
        format!("# pgbx config (ADR 0003). Profiles hold $VAR references, never values. Mode 0600: it names commands pgbx runs.\n{body}")
    }

    /// The command line of adapter `name`.
    pub fn adapter_argv(&self, name: &str, dir: &Path) -> Result<Vec<String>, String> {
        let v = self.adapters.get(name).ok_or_else(|| {
            format!("no adapter '{name}' in {} (add `adapters: {name}: <command>`, or pgbx profile add ... --adapter {name} --adapter-command '<command>')", FILE)
        })?;
        argv(v, dir).map_err(|e| format!("adapter '{name}': {e}"))
    }
}

pub fn path(env: Env) -> Result<PathBuf, String> {
    Ok(config_dir(env)?.join(FILE))
}

/// Load config.yaml (migrating profiles.json first if that is all there is). Notices are for the user, once.
pub fn load(env: Env) -> Result<(Config, Vec<String>), String> {
    let p = path(env)?;
    match std::fs::read_to_string(&p) {
        Ok(s) => Ok((Config::from_yaml(&s).map_err(|e| format!("{}: {e}", p.display()))?, vec![])),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => migrate(env),
        Err(e) => Err(format!("cannot read {}: {e}", p.display())),
    }
}

/// profiles.json (pgbx <= 0.5) -> config.yaml. Nothing to migrate: an empty config, nothing written.
fn migrate(env: Env) -> Result<(Config, Vec<String>), String> {
    let dir = config_dir(env)?;
    let old = dir.join("profiles.json");
    let text = match std::fs::read_to_string(&old) {
        Ok(t) => t,
        Err(_) => return Ok((Config::default(), vec![])),
    };
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("{}: not valid JSON ({e}); fix or remove it", old.display()))?;
    let (cfg, ssh) = convert(&v, env)?;
    save(env, &cfg)?;
    let kept = dir.join("profiles.json.migrated");
    std::fs::rename(&old, &kept).map_err(|e| format!("cannot rename {}: {e}", old.display()))?;
    let mut notes = vec![format!(
        "moved {} profile(s) from {} to {} (the old file is kept as {}); profiles now hold a url or an adapter",
        cfg.profiles.len(), old.display(), path(env)?.display(), kept.display())];
    if ssh > 0 {
        let cmd = cfg.adapters.get("ssh").map(|c| match c {
            Value::String(s) => s.clone(),
            x => x.to_string(),
        }).unwrap_or_default();
        notes.push(format!(
            "pgbx no longer has built-in SSH: {ssh} ssh profile(s) now use the ssh example adapter `{cmd}` (needs Node 18+). \
             If that file is missing, copy adapters/ from the pgbx repo or release to {}",
            adapters_dir(env)?.display()));
    }
    Ok((cfg, notes))
}

/// Convert the old JSON store. Returns the config and how many profiles used ssh.
pub fn convert(v: &Value, env: Env) -> Result<(Config, usize), String> {
    let mut cfg = Config { default: v["default"].as_str().map(String::from), ..Default::default() };
    let mut ssh = 0;
    let s = |p: &Value, k: &str| p.get(k).and_then(|x| x.as_str()).filter(|x| !x.is_empty()).map(String::from);
    for (name, p) in v["profiles"].as_object().cloned().unwrap_or_default() {
        let mut out = Map::new();
        for k in ["admin-db", "s3-endpoint", "s3-bucket", "s3-region", "server-name", "credentials-file"] {
            if let Some(x) = s(&p, k) {
                out.insert(k.into(), json!(x));
            }
        }
        if let Some(target) = s(&p, "ssh") {
            ssh += 1;
            out.insert("adapter".into(), json!("ssh"));
            out.insert("target".into(), json!(target));
            if let Some(x) = s(&p, "ssh-port") {
                out.insert("ssh_port".into(), json!(x));
            }
            if let Some(x) = s(&p, "ssh-jump") {
                out.insert("jump".into(), json!(x));
            }
            if let Some(h) = s(&p, "host").filter(|h| !h.starts_with('/')) {
                out.insert("pg_host".into(), json!(h));
            }
            if let Some(x) = s(&p, "port") {
                out.insert("pg_port".into(), json!(x));
            }
            if let Some(x) = s(&p, "user") {
                out.insert("user".into(), json!(x));
            }
            if !cfg.adapters.contains_key("ssh") {
                cfg.adapters.insert("ssh".into(), example_command(env, "ssh")?);
            }
        } else {
            let host = s(&p, "host").unwrap_or_else(|| crate::DEFAULT_HOST.to_string());
            let host = if host.starts_with('/') { crate::vars::pct(&host) } else { host };
            let user = s(&p, "user").map(|u| format!("{}@", crate::vars::pct(&u))).unwrap_or_default();
            let port = s(&p, "port").map(|x| format!(":{x}")).unwrap_or_default();
            out.insert("url".into(), json!(format!("postgres://{user}{host}{port}/")));
        }
        cfg.profiles.insert(name, Value::Object(out));
    }
    Ok((cfg, ssh))
}

/// Write config.yaml atomically (temp file + rename), mode 0600, its directory 0700.
pub fn save(env: Env, cfg: &Config) -> Result<PathBuf, String> {
    let p = path(env)?;
    let dir = p.parent().unwrap();
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let tmp = p.with_extension(format!("yaml.tmp{}", std::process::id()));
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    use std::io::Write;
    let mut f = o.open(&tmp).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    f.write_all(cfg.to_yaml().as_bytes()).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    drop(f);
    std::fs::rename(&tmp, &p).map_err(|e| format!("cannot write {}: {e}", p.display()))?;
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pgbx-cfg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn yaml_round_trip() {
        let y = "default: prod\nsecrets: .secrets/.env\nadapters:\n  ssh: node ~/a/ssh-adapter.js\n  corp: [/usr/local/bin/corp pg, --fast]\n\
                 profiles:\n  prod:\n    adapter: ssh\n    target: ops@db1\n    pg_port: 5432\n    password: $PGPASSWORD\n\
                 \x20 dev:\n    url: postgres://$PGUSER:${PGPASSWORD}@localhost/shop\n";
        let c = Config::from_yaml(y).unwrap();
        assert_eq!(c.default.as_deref(), Some("prod"));
        assert_eq!(c.profiles["prod"]["pg_port"], 5432);
        assert_eq!(c.profiles["prod"]["password"], "$PGPASSWORD");
        assert_eq!(c.profiles["dev"]["url"], "postgres://$PGUSER:${PGPASSWORD}@localhost/shop");
        assert_eq!(c.adapter_argv("corp", Path::new("/")).unwrap(), ["/usr/local/bin/corp pg", "--fast"]);
        assert!(c.adapter_argv("ssh", Path::new("/")).unwrap()[1].ends_with("/a/ssh-adapter.js"));
        assert!(c.adapter_argv("nope", Path::new("/")).unwrap_err().contains("no adapter 'nope'"));
        let back = Config::from_yaml(&c.to_yaml()).unwrap();
        assert_eq!(back, c);
        assert!(c.to_yaml().starts_with("# pgbx config"));
        assert_eq!(Config::from_yaml("").unwrap(), Config::default());
        assert!(Config::from_yaml("profile: {}").unwrap_err().contains("unknown top-level key"));
        assert!(Config::from_yaml("profiles: [a]").is_err());
        assert!(Config::from_yaml("profiles:\n  x: 5\n").unwrap_err().contains("profile 'x'"));
        assert!(Config::from_yaml(": : :").is_err());
    }

    #[test]
    fn words() {
        assert_eq!(split_words("node /a/b.js --x 'y z' \"q \\\" r\"").unwrap(), ["node", "/a/b.js", "--x", "y z", "q \" r"]);
        assert_eq!(split_words("  a  ''  b").unwrap(), ["a", "", "b"]);
        assert!(split_words("a 'b").is_err());
    }

    #[test]
    fn migrates_profiles_json_once() {
        let d = tmpdir("mig");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("profiles.json"), r#"{"default":"prod","profiles":{
            "prod":{"ssh":"ops@db1","ssh-port":"2222","ssh-jump":"bastion","host":"10.0.0.5","port":"6432","user":"app","tunnel-idle":"5m","s3-bucket":"b"},
            "local":{"host":"/var/run/postgresql","user":"postgres"},
            "dev":{"host":"localhost","port":"5433","user":"me","admin-db":"admin"}}}"#).unwrap();
        let ds = d.display().to_string();
        let env = move |k: &str| (k == "PGBX_CONFIG_DIR").then(|| ds.clone());
        let (c, notes) = load(&env).unwrap();
        assert_eq!(c.default.as_deref(), Some("prod"));
        let p = &c.profiles["prod"];
        assert_eq!((p["adapter"].as_str(), p["target"].as_str(), p["ssh_port"].as_str(), p["jump"].as_str()),
                   (Some("ssh"), Some("ops@db1"), Some("2222"), Some("bastion")));
        assert_eq!((p["pg_host"].as_str(), p["pg_port"].as_str(), p["user"].as_str(), p["s3-bucket"].as_str()),
                   (Some("10.0.0.5"), Some("6432"), Some("app"), Some("b")));
        assert!(p.get("tunnel-idle").is_none() && p.get("ssh").is_none());
        assert_eq!(c.profiles["local"]["url"], "postgres://postgres@%2Fvar%2Frun%2Fpostgresql/");
        assert_eq!(c.profiles["dev"]["url"], "postgres://me@localhost:5433/");
        assert_eq!(c.profiles["dev"]["admin-db"], "admin");
        assert!(c.adapters["ssh"].as_str().unwrap().ends_with("/adapters/ssh/ssh-adapter.js"));
        assert_eq!(notes.len(), 2);
        assert!(notes[1].contains("no longer has built-in SSH"));
        assert!(d.join("config.yaml").exists() && d.join("profiles.json.migrated").exists() && !d.join("profiles.json").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(d.join("config.yaml")).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let local: postgres::Config = c.profiles["local"]["url"].as_str().unwrap().parse().unwrap();
        #[cfg(unix)]
        assert!(matches!(&local.get_hosts()[0], postgres::config::Host::Unix(p) if p == Path::new("/var/run/postgresql")));
        let _ = local;
        // second load: from config.yaml, no notices
        let (c2, notes2) = load(&env).unwrap();
        assert_eq!(c2, c);
        assert!(notes2.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn nothing_to_migrate_writes_nothing() {
        let d = tmpdir("none");
        let ds = d.display().to_string();
        let env = move |k: &str| (k == "PGBX_CONFIG_DIR").then(|| ds.clone());
        let (c, n) = load(&env).unwrap();
        assert_eq!((c, n.len()), (Config::default(), 0));
        assert!(!d.exists());
    }

    #[test]
    fn config_dir_resolution() {
        let x = |k: &str| match k { "XDG_CONFIG_HOME" => Some("/x".into()), "HOME" => Some("/h".into()), _ => None };
        let h = |k: &str| (k == "HOME").then(|| "/h".to_string());
        let none = |_: &str| None;
        if !cfg!(windows) {
            assert_eq!(config_dir(&x).unwrap(), PathBuf::from("/x/pgbx"));
            assert_eq!(config_dir(&h).unwrap(), PathBuf::from("/h/.config/pgbx"));
            assert_eq!(adapters_dir(&h).unwrap(), PathBuf::from("/h/.config/pgbx/adapters"));
        }
        assert!(config_dir(&none).is_err());
    }
}
