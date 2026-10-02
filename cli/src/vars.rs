//! `$VAR` / `${VAR}` expansion and redaction (ADR 0003). Profiles and connection strings hold references, never
//! values; pgbx expands them at run time, uses the values in memory only and scrubs them from everything it prints.
//!
//! Each variable comes from the process environment first, then from the `secrets:` source in config.yaml:
//!   env              (the default) the environment only
//!   path/to/.env     a .env file: KEY=value lines, `#` comments, optional quotes (relative to the config dir)
//!   a command        run once per variable as `<command> NAME` (no shell): stdout is the value, 10 s timeout;
//!                    a non-zero exit or empty output is an error naming the variable, never a value
//! `$$` is a literal `$`. A `$` not followed by a name, `{` or `$` stays as it is.

use serde_json::Value;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const HANDLER_TIMEOUT: Duration = Duration::from_secs(10);

/// Where `$VAR` values come from after the environment.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    Env,
    File(PathBuf),
    Command(Vec<String>),
}

impl Source {
    /// `secrets:` from config.yaml: "env", a .env path, or a command line. `dir` is the config file's directory.
    pub fn parse(v: Option<&Value>, dir: &Path) -> Result<Source, String> {
        let s = match v {
            None | Some(Value::Null) => return Ok(Source::Env),
            Some(Value::String(s)) => s.trim().to_string(),
            Some(Value::Array(_)) => return Ok(Source::Command(crate::config::argv(v.unwrap(), dir)?)),
            Some(_) => return Err("secrets: must be `env`, a .env file path or a command".into()),
        };
        if s.is_empty() || s == "env" {
            return Ok(Source::Env);
        }
        let file_name = Path::new(&s).file_name().and_then(|f| f.to_str()).unwrap_or("");
        if !s.contains(char::is_whitespace) && (file_name.ends_with(".env") || file_name.starts_with(".env")) {
            let p = crate::config::tilde(&s);
            return Ok(Source::File(if Path::new(&p).is_absolute() { PathBuf::from(p) } else { dir.join(p) }));
        }
        Ok(Source::Command(crate::config::argv(&Value::String(s), dir)?))
    }

    pub fn describe(&self) -> String {
        match self {
            Source::Env => "the environment".into(),
            Source::File(p) => format!("the environment, then {}", p.display()),
            Source::Command(c) => format!("the environment, then the secret handler `{}`", c.join(" ")),
        }
    }
}

/// Parse a .env file: KEY=value, `export KEY=value`, `#` comments, blank lines, optional single or double quotes.
pub fn parse_dotenv(text: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let l = l.strip_prefix("export ").unwrap_or(l);
        let Some((k, v)) = l.split_once('=') else { continue };
        let k = k.trim();
        if !is_name(k) {
            continue;
        }
        let v = v.trim();
        let v = if v.len() >= 2 && ((v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\''))) {
            v[1..v.len() - 1].to_string()
        } else {
            // an unquoted value ends at an inline comment (" #")
            v.split(" #").next().unwrap_or("").trim_end().to_string()
        };
        m.insert(k.to_string(), v);
    }
    m
}

fn is_name(s: &str) -> bool {
    let mut c = s.chars();
    matches!(c.next(), Some(x) if x.is_ascii_alphabetic() || x == '_') && c.all(|x| x.is_ascii_alphanumeric() || x == '_')
}

/// Names that look like secrets: their env values are scrubbed from output too.
pub fn secret_name(n: &str) -> bool {
    let u = n.to_ascii_uppercase();
    ["PASS", "PWD", "SECRET", "TOKEN", "KEY", "CREDENTIAL", "AUTH"].iter().any(|w| u.contains(w))
}

/// Run a secret handler for one variable: `<argv...> NAME`, stdout = value (one trailing newline stripped).
pub fn run_handler(argv: &[String], name: &str, cwd: &Path, timeout: Duration) -> Result<String, String> {
    let cmd = argv.first().ok_or("secrets: empty command")?;
    let mut c = Command::new(cmd);
    c.args(&argv[1..]).arg(name).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if cwd.is_dir() {
        c.current_dir(cwd);
    }
    crate::adapter::own_group(&mut c);
    let mut child = c.spawn().map_err(|e| format!("cannot run the secret handler `{cmd}` for ${name}: {e}"))?;
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let ro = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = out.read_to_end(&mut b);
        b
    });
    let re = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = err.by_ref().take(64 * 1024).read_to_end(&mut b);
        b
    });
    let t0 = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().map_err(|e| e.to_string())? {
            break s;
        }
        if t0.elapsed() > timeout {
            crate::adapter::kill_tree(&mut child);
            return Err(format!("the secret handler `{cmd}` did not answer for ${name} within {} s", timeout.as_secs_f32()));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let value = String::from_utf8_lossy(&ro.join().unwrap_or_default()).to_string();
    let stderr = String::from_utf8_lossy(&re.join().unwrap_or_default()).to_string();
    let value = value.strip_suffix('\n').map(|v| v.strip_suffix('\r').unwrap_or(v)).unwrap_or(&value).to_string();
    if !value.is_empty() {
        register(&value); // whatever happens next, it never reaches output
    }
    if !status.success() {
        return Err(format!("the secret handler `{cmd}` failed for ${name} ({status}){}", tail(&stderr)));
    }
    if value.is_empty() {
        return Err(format!("the secret handler `{cmd}` printed nothing for ${name}"));
    }
    Ok(value)
}

fn tail(stderr: &str) -> String {
    let t = scrub(stderr.trim());
    if t.is_empty() {
        String::new()
    } else {
        format!(": {}", t.chars().rev().take(300).collect::<Vec<_>>().into_iter().rev().collect::<String>())
    }
}

/// Resolves variables: environment first, then the configured source. Values found are cached for this run.
pub struct Resolver<'a> {
    pub env: &'a dyn Fn(&str) -> Option<String>,
    pub source: Source,
    /// the config dir: the secret handler's working directory
    pub dir: PathBuf,
    pub timeout: Duration,
    file: Option<HashMap<String, String>>,
    cache: HashMap<String, String>,
}

impl<'a> Resolver<'a> {
    pub fn new(env: &'a dyn Fn(&str) -> Option<String>, source: Source, dir: PathBuf) -> Resolver<'a> {
        Resolver { env, source, dir, timeout: HANDLER_TIMEOUT, file: None, cache: HashMap::new() }
    }

    pub fn get(&mut self, name: &str) -> Result<String, String> {
        if let Some(v) = self.cache.get(name) {
            return Ok(v.clone());
        }
        let v = match (self.env)(name).filter(|v| !v.is_empty()) {
            Some(v) => {
                if secret_name(name) {
                    register(&v);
                }
                v
            }
            None => match &self.source {
                Source::Env => return Err(format!("${name} is not set (looked in {})", self.source.describe())),
                Source::File(p) => {
                    if self.file.is_none() {
                        let t = std::fs::read_to_string(p).map_err(|e| format!("secrets: cannot read {}: {e}", p.display()))?;
                        let m = parse_dotenv(&t);
                        m.values().for_each(|v| register(v));
                        self.file = Some(m);
                    }
                    match self.file.as_ref().and_then(|m| m.get(name)).filter(|v| !v.is_empty()) {
                        Some(v) => v.clone(),
                        None => return Err(format!("${name} is not set (looked in {})", self.source.describe())),
                    }
                }
                Source::Command(c) => run_handler(c, name, &self.dir, self.timeout)?,
            },
        };
        self.cache.insert(name.to_string(), v.clone());
        Ok(v)
    }

    /// Expand `$VAR`, `${VAR}` and `$$` in `s`. `enc` percent-encodes each value (for the userinfo of a URL).
    pub fn expand_with(&mut self, s: &str, enc: bool) -> Result<String, String> {
        let b = s.as_bytes();
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] != b'$' {
                let ch = s[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
                continue;
            }
            let rest = &s[i + 1..];
            if rest.starts_with('$') {
                out.push('$');
                i += 2;
            } else if let Some(r) = rest.strip_prefix('{') {
                let end = r.find('}').ok_or_else(|| format!("unterminated ${{ in a profile value (near position {i})"))?;
                let name = &r[..end];
                if !is_name(name) {
                    return Err(format!("bad variable name '${{{name}}}'"));
                }
                let v = self.get(name)?;
                out.push_str(&if enc { pct(&v) } else { v });
                i += 3 + end;
            } else {
                let n = rest.bytes().take_while(|c| c.is_ascii_alphanumeric() || *c == b'_').count();
                if n == 0 || rest.as_bytes()[0].is_ascii_digit() {
                    out.push('$'); // not a reference
                    i += 1;
                    continue;
                }
                let v = self.get(&rest[..n])?;
                out.push_str(&if enc { pct(&v) } else { v });
                i += 1 + n;
            }
        }
        Ok(out)
    }

    pub fn expand(&mut self, s: &str) -> Result<String, String> {
        self.expand_with(s, false)
    }

    /// A connection string: values inside the `user:password@` part of a URL are percent-encoded, so a password
    /// with `@`, `:` or `/` in it still parses. Key=value strings are expanded as they are.
    pub fn expand_url(&mut self, s: &str) -> Result<String, String> {
        if let Some((scheme, rest)) = s.split_once("://") {
            let auth_end = rest.find(['/', '?']).unwrap_or(rest.len());
            if let Some(at) = rest[..auth_end].rfind('@') {
                let userinfo = self.expand_with(&rest[..at], true)?;
                let tail = self.expand(&rest[at..])?;
                return Ok(format!("{scheme}://{userinfo}{tail}"));
            }
        }
        self.expand(s)
    }

    /// Expand every string inside a JSON value (keys stay as they are).
    pub fn expand_value(&mut self, v: &Value) -> Result<Value, String> {
        Ok(match v {
            Value::String(s) => Value::String(self.expand(s)?),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.expand_value(x)).collect::<Result<_, _>>()?),
            Value::Object(m) => {
                let mut o = serde_json::Map::new();
                for (k, x) in m {
                    o.insert(k.clone(), self.expand_value(x)?);
                }
                Value::Object(o)
            }
            x => x.clone(),
        })
    }
}

/// Percent-encode everything but unreserved characters (RFC 3986).
pub fn pct(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// True when `s` is exactly one `$NAME` / `${NAME}` reference (a reference is not a secret).
pub fn is_reference(s: &str) -> bool {
    if let Some(r) = s.strip_prefix("${").and_then(|r| r.strip_suffix('}')) {
        return is_name(r);
    }
    s.strip_prefix('$').is_some_and(is_name)
}

/// A value for a secret field (a password, a token) that is not a single `$VAR` reference: pgbx refuses to store it.
pub fn is_literal_secret(s: &str) -> bool {
    !s.is_empty() && !is_reference(s.trim())
}

// ---------------------------------------------------------------- redaction

static SECRETS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Remember a value that must never be printed (passwords, expanded secrets, adapter URLs' passwords).
pub fn register(v: &str) {
    if v.len() < 3 {
        return; // too short to scrub without mangling ordinary text
    }
    let mut g = SECRETS.lock().unwrap_or_else(|e| e.into_inner());
    if !g.iter().any(|x| x == v) {
        g.push(v.to_string());
        let enc = pct(v);
        if enc != v {
            g.push(enc);
        }
        g.sort_by_key(|x| std::cmp::Reverse(x.len()));
    }
}

/// `s` with every registered secret replaced by `***` and every URL / key=value password masked.
pub fn scrub(s: &str) -> String {
    let mut out = redact_conn(s);
    for v in SECRETS.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        if out.contains(v.as_str()) {
            out = out.replace(v.as_str(), "***");
        }
    }
    out
}

/// Mask passwords in any connection strings inside `s`: `scheme://user:PASS@` and `password=PASS`.
pub fn redact_conn(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        // the authority ends at whitespace, a quote, '/', '?' or '#'
        let end = tail.find(|c: char| c.is_whitespace() || "\"'/?#,;)>]}".contains(c)).unwrap_or(tail.len());
        let auth = &tail[..end];
        match auth.rfind('@') {
            Some(at) => {
                let userinfo = &auth[..at];
                match userinfo.split_once(':') {
                    Some((u, p)) if !p.is_empty() && !is_reference(p) => out.push_str(&format!("{u}:***{}", &auth[at..])),
                    _ => out.push_str(auth),
                }
            }
            None => out.push_str(auth),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    // key=value form: password=... up to whitespace (or a quoted value)
    let mut res = String::with_capacity(out.len());
    let mut r = out.as_str();
    while let Some(i) = r.find("password=") {
        let (head, tail) = r.split_at(i + 9);
        res.push_str(head);
        let end = if let Some(q) = tail.strip_prefix('\'') {
            q.find('\'').map(|e| e + 2).unwrap_or(tail.len())
        } else {
            tail.find(|c: char| c.is_whitespace() || c == '"' || c == '&').unwrap_or(tail.len())
        };
        let v = &tail[..end];
        if v.is_empty() || is_reference(v) {
            res.push_str(v);
        } else {
            res.push_str("***");
        }
        r = &tail[end..];
    }
    res.push_str(r);
    res
}

/// Every string in a JSON value, scrubbed.
pub fn scrub_value(v: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(scrub(s)),
        Value::Array(a) => Value::Array(a.iter().map(scrub_value).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (k.clone(), scrub_value(x))).collect()),
        x => x.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pgbx-vars-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn expands_env_braces_and_dollar_dollar() {
        let env = env_of(&[("PGUSER", "app"), ("PGPASSWORD", "s3cr3t-pw"), ("HOST_1", "db1")]);
        let mut r = Resolver::new(&env, Source::Env, PathBuf::from("."));
        assert_eq!(r.expand("$PGUSER@${HOST_1}:5432").unwrap(), "app@db1:5432");
        assert_eq!(r.expand("price $$5 and $$PGUSER").unwrap(), "price $5 and $PGUSER");
        assert_eq!(r.expand("a $ b $1 c$").unwrap(), "a $ b $1 c$");
        assert_eq!(r.expand("${PGUSER}x").unwrap(), "appx");
        assert!(r.expand("${PGUSER").unwrap_err().contains("unterminated"));
        assert!(r.expand("${1x}").unwrap_err().contains("bad variable"));
        let e = r.expand("postgres://$PGUSER:$MISSING_PW@h/db").unwrap_err();
        assert!(e.contains("$MISSING_PW is not set") && e.contains("environment"), "{e}");
        assert!(!e.contains("app"));
    }

    #[test]
    fn url_userinfo_is_percent_encoded() {
        let env = env_of(&[("U", "ops"), ("P", "p@ss:w/rd?#")]);
        let mut r = Resolver::new(&env, Source::Env, PathBuf::from("."));
        let u = r.expand_url("postgres://$U:$P@db.example:5432/shop?sslmode=disable").unwrap();
        assert_eq!(u, "postgres://ops:p%40ss%3Aw%2Frd%3F%23@db.example:5432/shop?sslmode=disable");
        let c: postgres::Config = u.parse().unwrap();
        assert_eq!(c.get_password(), Some(&b"p@ss:w/rd?#"[..]));
        assert_eq!(r.expand_url("host=h user=$U").unwrap(), "host=h user=ops");
    }

    #[test]
    fn dotenv_parsing_and_source() {
        let m = parse_dotenv("# c\n\nA=1\nexport B = two \nC=\"q v\"\nD='s v'\nE=x #comment\nbad line\n1X=no\n");
        assert_eq!(m["A"], "1");
        assert_eq!(m["B"], "two");
        assert_eq!(m["C"], "q v");
        assert_eq!(m["D"], "s v");
        assert_eq!(m["E"], "x");
        assert!(!m.contains_key("1X"));
        let d = tmp("env");
        std::fs::write(d.join("secrets.env"), "DB_PW=from-file-pw\n").unwrap();
        let src = Source::parse(Some(&Value::String("secrets.env".into())), &d).unwrap();
        assert_eq!(src, Source::File(d.join("secrets.env")));
        let env = env_of(&[("DB_PW", "from-env-pw")]);
        let mut r = Resolver::new(&env, src.clone(), d.clone());
        assert_eq!(r.expand("$DB_PW").unwrap(), "from-env-pw", "the environment wins");
        let none = |_: &str| None;
        let mut r = Resolver::new(&none, src, d.clone());
        assert_eq!(r.expand("$DB_PW").unwrap(), "from-file-pw");
        let e = r.expand("$NOPE").unwrap_err();
        assert!(e.contains("$NOPE") && e.contains("secrets.env"), "{e}");
        assert_eq!(Source::parse(Some(&Value::String("env".into())), &d).unwrap(), Source::Env);
        assert_eq!(Source::parse(None, &d).unwrap(), Source::Env);
        assert!(matches!(Source::parse(Some(&Value::String("node handler.js".into())), &d).unwrap(), Source::Command(c) if c == ["node", "handler.js"]));
        assert!(matches!(Source::parse(Some(&Value::String(".secrets/.env".into())), &d).unwrap(), Source::File(_)));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn secret_handler_ok_missing_empty_failing_and_hanging() {
        let d = tmp("handler");
        let h = d.join("handler.sh");
        std::fs::write(&h, "#!/bin/sh\ncase \"$1\" in\n  GOOD) echo 'handler-value-42';;\n  EMPTY) ;;\n  FAIL) echo 'vault says no' >&2; exit 3;;\n  HANG) sleep 30;;\n  *) exit 1;;\nesac\n").unwrap();
        let src = Source::parse(Some(&Value::String("sh handler.sh".into())), &d).unwrap();
        let none = |_: &str| None;
        let mut r = Resolver::new(&none, src, d.clone());
        r.timeout = Duration::from_millis(800);
        assert_eq!(r.expand("x-$GOOD").unwrap(), "x-handler-value-42");
        assert_eq!(scrub("leak handler-value-42 here"), "leak *** here");
        let e = r.expand("$EMPTY").unwrap_err();
        assert!(e.contains("printed nothing for $EMPTY"), "{e}");
        let e = r.expand("$FAIL").unwrap_err();
        assert!(e.contains("failed for $FAIL") && e.contains("vault says no"), "{e}");
        let t0 = Instant::now();
        let e = r.expand("$HANG").unwrap_err();
        assert!(e.contains("did not answer for $HANG"), "{e}");
        assert!(t0.elapsed() < Duration::from_secs(5), "the hanging handler is killed at the timeout");
        let e = r.expand("$OTHER").unwrap_err();
        assert!(e.contains("$OTHER"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn redaction() {
        assert_eq!(redact_conn("postgres://ops:hunter2@127.0.0.1:5/shop"), "postgres://ops:***@127.0.0.1:5/shop");
        assert_eq!(redact_conn("x postgresql://ops@h/db y"), "x postgresql://ops@h/db y");
        assert_eq!(redact_conn("postgres://ops:$PGPASSWORD@h/db"), "postgres://ops:$PGPASSWORD@h/db");
        assert_eq!(redact_conn("err: \"postgres://a:b%40c@h:1/d?sslmode=require\""), "err: \"postgres://a:***@h:1/d?sslmode=require\"");
        assert_eq!(redact_conn("host=h password=pw1 user=u"), "host=h password=*** user=u");
        assert_eq!(redact_conn("password='p w' x"), "password=*** x");
        assert_eq!(redact_conn("password=$PGPASSWORD"), "password=$PGPASSWORD");
        register("zz-registered-secret");
        assert_eq!(scrub("a zz-registered-secret b"), "a *** b");
        assert_eq!(scrub(&pct("zz-registered-secret")), "***");
        register("ab"); // too short to register
        assert_eq!(scrub("ab"), "ab");
        assert!(is_reference("$PGPASSWORD") && is_reference("${X_1}") && !is_reference("pw") && !is_reference("$1"));
        assert!(secret_name("PGPASSWORD") && secret_name("api_token") && !secret_name("PGUSER"));
        let v = scrub_value(&serde_json::json!({"e": ["postgres://u:p4ss@h/d"]}));
        assert_eq!(v["e"][0], "postgres://u:***@h/d");
    }
}
