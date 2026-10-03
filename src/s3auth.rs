//! S3 credentials without a keys file: the AWS default chain, for the extension and the CLI (no pgrx in here, so
//! both crates compile this file, see `#[path]` in cli/src/main.rs).
//!
//! `pgbx.credentials_file` (and the CLI's `--credentials-file` / `credentials_file=` in pgbx-wal.conf):
//!   * a path: `access_key_id=` / `secret_access_key=` lines, read by the caller exactly as before 0.6;
//!   * empty or `aws-default`: the first of these that is configured answers (a configured source that fails is an
//!     error, never a silent fall-through to the next):
//!       1. env AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY (+ AWS_SESSION_TOKEN): only where the process environment
//!          is the user's (the CLI); the extension's worker never takes keys from the postmaster's environment;
//!       2. web identity (EKS IRSA): AWS_WEB_IDENTITY_TOKEN_FILE + AWS_ROLE_ARN -> STS AssumeRoleWithWebIdentity;
//!       3. container credentials (ECS, EKS Pod Identity): AWS_CONTAINER_CREDENTIALS_RELATIVE_URI / _FULL_URI;
//!       4. the EC2 instance role through IMDSv2: PUT /latest/api/token, then GET
//!          /latest/meta-data/iam/security-credentials/<role> with that token. IMDSv1 (no token) is never used.
//!          AWS_EC2_METADATA_SERVICE_ENDPOINT moves the endpoint (tests, IPv6); AWS_EC2_METADATA_DISABLED=true skips it.
//!
//! Temporary credentials are cached per process and fetched again 5 minutes before they expire, or after S3 answers
//! 403 (ExpiredToken and friends). A bucket made from them is brought up to date by `fresh` before each request, so a
//! long multipart upload goes on across a refresh (an upload id is not tied to the credentials that started it).
//!
//! No key, secret or token ever goes into an error or a log line: errors name the source and what failed.
//!
//! aws-creds (rust-s3's credentials crate) has `from_instance_metadata_v2`, but with a fixed endpoint (no
//! AWS_EC2_METADATA_SERVICE_ENDPOINT), a fall-back to IMDSv1, ~/.aws profiles in its chain, and its own `refresh`
//! that reruns that whole chain only once the credentials have already expired; so the chain is here, on the HTTP
//! client rust-s3 already brings (attohttpc). Only aws-creds' `Credentials` type is used.

use chrono::{DateTime, Duration as Span, Utc};
use s3::creds::Credentials;
use s3::Bucket;
use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// The setting value that picks the AWS default chain (an empty setting does too).
pub const AWS_DEFAULT: &str = "aws-default";
/// Fetch new credentials this long before the old ones expire.
pub const EARLY_SECS: i64 = 300;
/// After a failed fetch, or one that brought nothing new, wait this long before asking again.
pub const RETRY_SECS: i64 = 10;
const DEFAULT_IMDS: &str = "http://169.254.169.254";
const ECS_HOST: &str = "http://169.254.170.2";

/// Does this credentials setting mean "the AWS default chain" (rather than a keys file)?
pub fn uses_chain(setting: &str) -> bool {
    let s = setting.trim();
    s.is_empty() || s == AWS_DEFAULT
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    File,
    Env,
    WebIdentity,
    Ecs,
    InstanceRole,
}

impl Source {
    /// Short name (doctor, JSON): file / env / web-identity / ecs / instance-role.
    pub fn name(self) -> &'static str {
        match self {
            Source::File => "file",
            Source::Env => "env",
            Source::WebIdentity => "web-identity",
            Source::Ecs => "ecs",
            Source::InstanceRole => "instance-role",
        }
    }
    /// What errors and doctor call it.
    pub fn label(self) -> &'static str {
        match self {
            Source::File => "credentials file",
            Source::Env => "environment (AWS_ACCESS_KEY_ID)",
            Source::WebIdentity => "web identity via STS",
            Source::Ecs => "container credentials",
            Source::InstanceRole => "instance role via IMDSv2",
        }
    }
}

/// One set of credentials and where it came from. Debug never shows the values.
#[derive(Clone, PartialEq, Eq)]
pub struct Creds {
    pub source: Source,
    /// e.g. the role name; never a secret
    pub detail: String,
    access_key: String,
    secret_key: String,
    token: Option<String>,
    pub expires: Option<DateTime<Utc>>,
}

impl std::fmt::Debug for Creds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Creds({}, {:?}, expires {:?}, <redacted>)", self.source.name(), self.detail, self.expires)
    }
}

impl Creds {
    /// Static keys (a keys file).
    pub fn keys(access_key: &str, secret_key: &str) -> Creds {
        Creds { source: Source::File, detail: String::new(), access_key: access_key.into(), secret_key: secret_key.into(), token: None, expires: None }
    }

    /// "instance role via IMDSv2 (role pgbx-backup), temporary, valid until 2026-10-03 13:00:00 UTC"
    pub fn describe(&self) -> String {
        let mut s = self.source.label().to_string();
        if !self.detail.is_empty() {
            s.push_str(&format!(" ({})", self.detail));
        }
        if let Some(e) = self.expires {
            s.push_str(&format!(", temporary, valid until {}", e.format("%Y-%m-%d %H:%M:%S UTC")));
        }
        s
    }

    /// For rust-s3. No expiration on purpose: rust-s3 would otherwise rerun aws-creds' own chain when it passes;
    /// `fresh` refreshes instead.
    pub fn s3(&self) -> Credentials {
        Credentials {
            access_key: Some(self.access_key.clone()),
            secret_key: Some(self.secret_key.clone()),
            security_token: self.token.clone(),
            session_token: None,
            expiration: None,
        }
    }
}

// ------------------------------------------------------------------------------------------- the environment

/// Everything the chain reads from outside, so tests can fake it.
pub trait World {
    fn var(&self, k: &str) -> Option<String>;
    fn read(&self, path: &str) -> Result<String, String>;
    /// (status, body). `Err` only when there was no HTTP answer at all.
    fn http(&self, method: &str, url: &str, headers: &[(&str, &str)], body: Option<&str>) -> Result<(u16, String), String>;
}

pub struct RealWorld;

impl World for RealWorld {
    fn var(&self, k: &str) -> Option<String> {
        std::env::var(k).ok().filter(|v| !v.trim().is_empty())
    }
    fn read(&self, path: &str) -> Result<String, String> {
        std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))
    }
    fn http(&self, method: &str, url: &str, headers: &[(&str, &str)], body: Option<&str>) -> Result<(u16, String), String> {
        let mut r = match method {
            "PUT" => attohttpc::put(url),
            "POST" => attohttpc::post(url),
            _ => attohttpc::get(url),
        }
        .connect_timeout(Duration::from_secs(2))
        .read_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .follow_redirects(false);
        for (k, v) in headers {
            let name = attohttpc::header::HeaderName::from_bytes(k.as_bytes()).map_err(|_| format!("bad header name {k}"))?;
            r = r.header(name, *v);
        }
        // attohttpc's errors carry no request body or header, only what went wrong on the wire
        let resp = match body {
            Some(b) => r.text(b.to_string()).send(),
            None => r.send(),
        }
        .map_err(|e| e.to_string())?;
        let code = resp.status().as_u16();
        Ok((code, resp.text().map_err(|e| e.to_string())?))
    }
}

// ------------------------------------------------------------------------------------------- the chain

fn src_err(s: Source, msg: impl std::fmt::Display) -> String {
    format!("s3 credentials ({}): {msg}", s.label())
}

/// The AWS default chain, once (no cache). `env_keys`: whether AWS_ACCESS_KEY_ID & co. count (the CLI).
pub fn chain(w: &dyn World, env_keys: bool) -> Result<Creds, String> {
    let mut skipped: Vec<&str> = vec![];
    if env_keys {
        match (w.var("AWS_ACCESS_KEY_ID"), w.var("AWS_SECRET_ACCESS_KEY")) {
            (Some(a), Some(s)) => {
                return Ok(Creds { source: Source::Env, detail: String::new(), access_key: a, secret_key: s, token: w.var("AWS_SESSION_TOKEN"), expires: None })
            }
            (None, None) => skipped.push("no AWS_ACCESS_KEY_ID in the environment"),
            _ => return Err(src_err(Source::Env, "AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY must both be set")),
        }
    }
    match (w.var("AWS_WEB_IDENTITY_TOKEN_FILE"), w.var("AWS_ROLE_ARN")) {
        (Some(f), Some(arn)) => return web_identity(w, &f, &arn).map_err(|e| src_err(Source::WebIdentity, e)),
        _ => skipped.push("no web identity (AWS_WEB_IDENTITY_TOKEN_FILE + AWS_ROLE_ARN)"),
    }
    if w.var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI").is_some() || w.var("AWS_CONTAINER_CREDENTIALS_FULL_URI").is_some() {
        return container(w).map_err(|e| src_err(Source::Ecs, e));
    }
    skipped.push("no container credentials (AWS_CONTAINER_CREDENTIALS_*_URI)");
    let nothing = |why: String| format!("s3 credentials: none found for pgbx.credentials_file = 'aws-default' ({}; {why})", skipped.join("; "));
    if w.var("AWS_EC2_METADATA_DISABLED").is_some_and(|v| v.eq_ignore_ascii_case("true")) {
        return Err(nothing("instance role: AWS_EC2_METADATA_DISABLED=true".into()));
    }
    imds(w).map_err(|e| nothing(format!("{}: {e}", Source::InstanceRole.label())))
}

/// EC2 instance metadata, IMDSv2 only.
fn imds(w: &dyn World) -> Result<Creds, String> {
    let base = w.var("AWS_EC2_METADATA_SERVICE_ENDPOINT").unwrap_or(DEFAULT_IMDS.into());
    let base = base.trim().trim_end_matches('/');
    let (code, token) = w
        .http("PUT", &format!("{base}/latest/api/token"), &[("X-aws-ec2-metadata-token-ttl-seconds", "21600")], None)
        .map_err(|e| {
            format!("no instance metadata service at {base} ({e}): not on EC2, or in a container on an instance whose \
                     metadata PUT hop limit is 1 (set it to 2)")
        })?;
    if code != 200 || token.trim().is_empty() {
        return Err(format!("the metadata service gave no IMDSv2 session token (HTTP {code}); pgbx never falls back to IMDSv1"));
    }
    let auth = [("X-aws-ec2-metadata-token", token.trim())];
    let list = format!("{base}/latest/meta-data/iam/security-credentials/");
    let (code, roles) = w.http("GET", &list, &auth, None).map_err(|e| format!("listing the instance role: {e}"))?;
    let role = roles.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").to_string();
    if code == 404 || (code == 200 && role.is_empty()) {
        return Err("no IAM role is attached to this instance (attach an instance profile)".into());
    }
    if code != 200 {
        return Err(format!("HTTP {code} listing the instance role"));
    }
    let (code, body) = w.http("GET", &format!("{list}{role}"), &auth, None).map_err(|e| format!("credentials of role {role}: {e}"))?;
    if code != 200 {
        return Err(format!("HTTP {code} fetching the credentials of role {role}"));
    }
    parse_json(&body, Source::InstanceRole, &format!("role {role}"))
}

/// ECS task role / EKS Pod Identity.
fn container(w: &dyn World) -> Result<Creds, String> {
    let url = match w.var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI") {
        Some(rel) => format!("{ECS_HOST}{rel}"),
        None => w.var("AWS_CONTAINER_CREDENTIALS_FULL_URI").unwrap_or_default(),
    };
    let auth = match (w.var("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE"), w.var("AWS_CONTAINER_AUTHORIZATION_TOKEN")) {
        (Some(f), _) => Some(w.read(&f)?.trim().to_string()),
        (None, t) => t,
    };
    let headers: Vec<(&str, &str)> = auth.as_deref().map(|a| vec![("Authorization", a)]).unwrap_or_default();
    let (code, body) = w.http("GET", &url, &headers, None).map_err(|e| format!("no answer from the credentials endpoint ({e})"))?;
    if code != 200 {
        return Err(format!("HTTP {code} from the credentials endpoint"));
    }
    parse_json(&body, Source::Ecs, "")
}

/// EKS IRSA: AssumeRoleWithWebIdentity (unsigned; the token goes in the POST body, never in a URL).
fn web_identity(w: &dyn World, token_file: &str, role_arn: &str) -> Result<Creds, String> {
    let token = w.read(token_file)?;
    let region = w.var("AWS_REGION").or_else(|| w.var("AWS_DEFAULT_REGION"));
    let url = match &region {
        Some(r) => format!("https://sts.{r}.amazonaws.com/"),
        None => "https://sts.amazonaws.com/".to_string(),
    };
    let session = w.var("AWS_ROLE_SESSION_NAME").unwrap_or("pgbx".into());
    let body = [
        ("Action", "AssumeRoleWithWebIdentity"),
        ("Version", "2011-06-15"),
        ("RoleArn", role_arn),
        ("RoleSessionName", &session),
        ("WebIdentityToken", token.trim()),
    ]
    .iter()
    .map(|(k, v)| format!("{k}={}", form_encode(v)))
    .collect::<Vec<_>>()
    .join("&");
    let (code, xml) = w
        .http("POST", &url, &[("Content-Type", "application/x-www-form-urlencoded")], Some(&body))
        .map_err(|e| format!("no answer from STS ({e})"))?;
    if code != 200 {
        return Err(format!("STS answered HTTP {code}{}", tag(&xml, "Code").map(|c| format!(" {c}")).unwrap_or_default()));
    }
    let get = |t: &str| tag(&xml, t).ok_or(format!("STS answer has no {t}"));
    Ok(Creds {
        source: Source::WebIdentity,
        detail: format!("role {role_arn}"),
        access_key: get("AccessKeyId")?,
        secret_key: get("SecretAccessKey")?,
        token: Some(get("SessionToken")?),
        expires: Some(rfc3339(&get("Expiration")?)?),
    })
}

fn form_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The text of the first <t>...</t> (STS answers are flat enough for this).
fn tag(xml: &str, t: &str) -> Option<String> {
    let open = format!("<{t}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&format!("</{t}>"))? + start;
    Some(xml[start..end].trim().to_string())
}

fn rfc3339(s: &str) -> Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(s.trim()).map(|t| t.with_timezone(&Utc)).map_err(|_| "unreadable Expiration".to_string())
}

/// IMDS / container credentials JSON: AccessKeyId, SecretAccessKey, Token, Expiration (+ Code on IMDS).
/// Errors name the missing field, never a value.
pub fn parse_json(body: &str, source: Source, detail: &str) -> Result<Creds, String> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|_| "the credentials answer is not JSON".to_string())?;
    if let Some(c) = v.get("Code").and_then(|c| c.as_str()).filter(|c| *c != "Success") {
        return Err(format!("the credentials answer says Code {c}"));
    }
    let get = |k: &str| v.get(k).and_then(|x| x.as_str()).filter(|x| !x.is_empty()).map(String::from).ok_or(format!("the credentials answer has no {k}"));
    Ok(Creds {
        source,
        detail: detail.to_string(),
        access_key: get("AccessKeyId")?,
        secret_key: get("SecretAccessKey")?,
        token: Some(get("Token")?),
        expires: Some(rfc3339(&get("Expiration")?)?),
    })
}

// ------------------------------------------------------------------------------------------- cache and refresh

/// Time to fetch new credentials? Static ones never are; temporary ones from EARLY_SECS before they expire.
pub fn due(c: &Creds, now: DateTime<Utc>) -> bool {
    c.expires.is_some_and(|e| now >= e - Span::seconds(EARLY_SECS))
}

/// The credentials one process uses, refreshed in time. The clock is passed in (tests).
#[derive(Default)]
pub struct Cache {
    cur: Option<Creds>,
    err: Option<String>,
    next_try: Option<DateTime<Utc>>,
    stale: bool,
    /// access key ids of temporary credentials handed out: buckets carrying one of these are refreshed by `fresh`
    issued: Vec<String>,
}

impl Cache {
    /// Credentials to use at `now`. Fetches through `fetch` when there are none yet, they are due, or S3 refused
    /// them (`mark_stale`); at most once per RETRY_SECS while the current ones still work.
    pub fn get(&mut self, now: DateTime<Utc>, fetch: impl FnOnce() -> Result<Creds, String>) -> Result<Creds, String> {
        let valid = |c: &Creds| c.expires.is_none_or(|e| now < e);
        if let Some(c) = self.cur.as_ref().filter(|c| !self.stale && !due(c, now)) {
            return Ok(c.clone());
        }
        if self.next_try.is_some_and(|t| now < t) {
            return match &self.cur {
                Some(c) if valid(c) => Ok(c.clone()),
                _ => Err(self.err.clone().unwrap_or("s3 credentials: none yet".into())),
            };
        }
        match fetch() {
            Ok(n) => {
                let same = self.cur.as_ref() == Some(&n);
                // the service may hand out the same credentials until it rotates them: do not ask on every request
                self.next_try = if due(&n, now) || (same && self.stale) { Some(now + Span::seconds(RETRY_SECS)) } else { None };
                self.stale = false;
                self.err = None;
                if n.expires.is_some() && !self.issued.contains(&n.access_key) {
                    self.issued.push(n.access_key.clone());
                    if self.issued.len() > 32 {
                        self.issued.remove(0);
                    }
                }
                self.cur = Some(n.clone());
                Ok(n)
            }
            Err(e) => {
                self.next_try = Some(now + Span::seconds(RETRY_SECS));
                self.err = Some(e.clone());
                match &self.cur {
                    Some(c) if valid(c) => Ok(c.clone()), // keep going on the old ones until they really expire
                    _ => Err(e),
                }
            }
        }
    }

    /// S3 refused the current credentials (403 ExpiredToken & co.): fetch again at the next `get`.
    pub fn mark_stale(&mut self) {
        if self.cur.as_ref().is_some_and(|c| c.expires.is_some()) {
            self.stale = true;
        }
    }

    pub fn issued(&self, access_key: &str) -> bool {
        self.issued.iter().any(|k| k == access_key)
    }
}

static CACHE: Mutex<Option<Cache>> = Mutex::new(None);
/// Set once temporary credentials were handed out: until then `fresh` is a no-op without a lock.
static ANY_TEMP: AtomicBool = AtomicBool::new(false);
static ENV_KEYS: AtomicBool = AtomicBool::new(false);
static ON_NEW: OnceLock<fn(&str)> = OnceLock::new();

/// The CLI: AWS_ACCESS_KEY_ID & co. count as a source (the extension never takes keys from its environment).
pub fn allow_env_keys() {
    ENV_KEYS.store(true, Ordering::Relaxed);
}

/// Called with a line (never a value) each time new temporary credentials are fetched.
pub fn on_new_credentials(f: fn(&str)) {
    let _ = ON_NEW.set(f);
}

fn with_cache<T>(f: impl FnOnce(&mut Cache) -> T) -> T {
    let mut g = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Cache::default))
}

/// Credentials from the AWS default chain, cached for this process.
pub fn from_chain() -> Result<Creds, String> {
    let (r, new) = with_cache(|c| {
        let before = c.cur.clone();
        let r = c.get(Utc::now(), || chain(&RealWorld, ENV_KEYS.load(Ordering::Relaxed)));
        let new = matches!(&r, Ok(n) if n.expires.is_some() && before.as_ref() != Some(n));
        (r, new)
    });
    if new {
        ANY_TEMP.store(true, Ordering::Relaxed);
        if let (Some(f), Ok(n)) = (ON_NEW.get(), &r) {
            f(&format!("s3 credentials from {}", n.describe()));
        }
    }
    r
}

/// Before a request: the bucket with up-to-date credentials. Buckets with static keys come back as they are.
pub fn fresh(b: &Bucket) -> Result<Cow<'_, Bucket>, String> {
    if !ANY_TEMP.load(Ordering::Relaxed) {
        return Ok(Cow::Borrowed(b));
    }
    let cur = b.credentials().map_err(|e| e.to_string())?;
    let ak = cur.access_key.clone().unwrap_or_default();
    if !with_cache(|c| c.issued(&ak)) {
        return Ok(Cow::Borrowed(b));
    }
    let n = from_chain()?.s3();
    if n == cur {
        return Ok(Cow::Borrowed(b));
    }
    let mut nb = b.clone();
    nb.set_credentials(n);
    Ok(Cow::Owned(nb))
}

/// After a failed request: if S3 refused temporary credentials, fetch new ones before the next attempt.
pub fn note_error(err: &str) {
    if !ANY_TEMP.load(Ordering::Relaxed) {
        return;
    }
    const REFUSED: [&str; 5] = ["ExpiredToken", "InvalidToken", "TokenRefreshRequired", "InvalidAccessKeyId", "403"];
    if REFUSED.iter().any(|m| err.contains(m)) {
        with_cache(|c| c.mark_stale());
    }
}

#[cfg(test)]
mod t {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    #[derive(Default)]
    struct Fake {
        vars: HashMap<String, String>,
        files: HashMap<String, String>,
        http: HashMap<(String, String), Result<(u16, String), String>>,
        calls: RefCell<Vec<String>>,
    }
    impl Fake {
        fn var(mut self, k: &str, v: &str) -> Self {
            self.vars.insert(k.into(), v.into());
            self
        }
        fn on(mut self, m: &str, url: &str, r: Result<(u16, &str), &str>) -> Self {
            self.http.insert((m.into(), url.into()), r.map(|(c, b)| (c, b.to_string())).map_err(String::from));
            self
        }
    }
    impl World for Fake {
        fn var(&self, k: &str) -> Option<String> {
            self.vars.get(k).cloned()
        }
        fn read(&self, p: &str) -> Result<String, String> {
            self.files.get(p).cloned().ok_or(format!("read {p}: not found"))
        }
        fn http(&self, m: &str, url: &str, h: &[(&str, &str)], _: Option<&str>) -> Result<(u16, String), String> {
            self.calls.borrow_mut().push(format!("{m} {url} {}", h.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(",")));
            self.http.get(&(m.to_string(), url.to_string())).cloned().unwrap_or(Err("connection refused".into()))
        }
    }

    const ROLE_JSON: &str = r#"{"Code":"Success","LastUpdated":"2026-10-03T10:00:00Z","Type":"AWS-HMAC",
        "AccessKeyId":"ASIAROLEKEY","SecretAccessKey":"rolesecret","Token":"roletoken","Expiration":"2026-10-03T16:00:00Z"}"#;
    const B: &str = "http://imds:80";

    fn imds_ok(f: Fake) -> Fake {
        f.var("AWS_EC2_METADATA_SERVICE_ENDPOINT", "http://imds:80/")
            .on("PUT", &format!("{B}/latest/api/token"), Ok((200, "tok")))
            .on("GET", &format!("{B}/latest/meta-data/iam/security-credentials/"), Ok((200, "pgbx-role\n")))
            .on("GET", &format!("{B}/latest/meta-data/iam/security-credentials/pgbx-role"), Ok((200, ROLE_JSON)))
    }

    #[test]
    fn uses_chain_for_empty_or_aws_default_only() {
        assert!(uses_chain("") && uses_chain("  ") && uses_chain("aws-default"));
        assert!(!uses_chain("/etc/pgbx/s3.credentials") && !uses_chain("AWS-DEFAULT"));
    }

    #[test]
    fn parses_instance_role_json() {
        let c = parse_json(ROLE_JSON, Source::InstanceRole, "role r").unwrap();
        assert_eq!(c.s3().access_key.as_deref(), Some("ASIAROLEKEY"));
        assert_eq!(c.s3().security_token.as_deref(), Some("roletoken"));
        assert_eq!(c.s3().expiration, None, "rust-s3 must never refresh by itself");
        assert_eq!(c.expires.unwrap().to_rfc3339(), "2026-10-03T16:00:00+00:00");
        // ECS answers have no Code
        assert!(parse_json(r#"{"AccessKeyId":"a","SecretAccessKey":"s","Token":"t","Expiration":"2026-10-03T16:00:00Z"}"#, Source::Ecs, "").is_ok());
        let e = parse_json(r#"{"Code":"Failure","AccessKeyId":"AKSECRETVALUE"}"#, Source::InstanceRole, "").unwrap_err();
        assert!(e.contains("Failure") && !e.contains("AKSECRETVALUE"));
        let e = parse_json(r#"{"AccessKeyId":"AKSECRETVALUE","SecretAccessKey":"SKVALUE","Expiration":"2026-10-03T16:00:00Z"}"#, Source::Ecs, "").unwrap_err();
        assert!(e.contains("no Token") && !e.contains("VALUE"));
        assert!(parse_json("<html>", Source::Ecs, "").unwrap_err().contains("not JSON"));
        let d = format!("{:?}", parse_json(ROLE_JSON, Source::InstanceRole, "role r").unwrap());
        assert!(!d.contains("ASIAROLEKEY") && !d.contains("rolesecret") && !d.contains("roletoken"), "{d}");
    }

    #[test]
    fn imdsv2_flow_token_header_and_role() {
        let f = imds_ok(Fake::default());
        let c = chain(&f, false).unwrap();
        assert_eq!(c.source, Source::InstanceRole);
        assert_eq!(c.detail, "role pgbx-role");
        let calls = f.calls.borrow();
        assert_eq!(calls[0], "PUT http://imds:80/latest/api/token X-aws-ec2-metadata-token-ttl-seconds");
        assert!(calls[1..].iter().all(|c| c.starts_with("GET ") && c.ends_with("X-aws-ec2-metadata-token")), "{calls:?}");
        assert!(c.describe().starts_with("instance role via IMDSv2 (role pgbx-role), temporary, valid until 2026-10-03 16:00:00 UTC"));
    }

    #[test]
    fn imdsv1_only_is_refused() {
        for code in [403u16, 404, 405] {
            let f = Fake::default()
                .var("AWS_EC2_METADATA_SERVICE_ENDPOINT", B)
                .on("PUT", &format!("{B}/latest/api/token"), Ok((code, "")))
                .on("GET", &format!("{B}/latest/meta-data/iam/security-credentials/"), Ok((200, "pgbx-role")))
                .on("GET", &format!("{B}/latest/meta-data/iam/security-credentials/pgbx-role"), Ok((200, ROLE_JSON)));
            let e = chain(&f, false).unwrap_err();
            assert!(e.contains(&format!("no IMDSv2 session token (HTTP {code})")) && e.contains("never falls back to IMDSv1"), "{e}");
            assert_eq!(f.calls.borrow().len(), 1, "no IMDSv1 GET after a refused token");
        }
    }

    #[test]
    fn imds_errors_name_the_problem() {
        let e = chain(&Fake::default(), true).unwrap_err();
        assert!(e.starts_with("s3 credentials: none found for pgbx.credentials_file = 'aws-default'"), "{e}");
        assert!(e.contains("no AWS_ACCESS_KEY_ID") && e.contains("no instance metadata service at http://169.254.169.254"), "{e}");
        let no_role = Fake::default()
            .var("AWS_EC2_METADATA_SERVICE_ENDPOINT", B)
            .on("PUT", &format!("{B}/latest/api/token"), Ok((200, "tok")))
            .on("GET", &format!("{B}/latest/meta-data/iam/security-credentials/"), Ok((404, "")));
        assert!(chain(&no_role, false).unwrap_err().contains("instance role via IMDSv2: no IAM role is attached to this instance"));
        let off = Fake::default().var("AWS_EC2_METADATA_DISABLED", "true");
        assert!(chain(&off, false).unwrap_err().contains("AWS_EC2_METADATA_DISABLED"));
        assert!(off.calls.borrow().is_empty());
    }

    #[test]
    fn source_order() {
        let all = imds_ok(Fake::default())
            .var("AWS_ACCESS_KEY_ID", "AKENV")
            .var("AWS_SECRET_ACCESS_KEY", "SKENV")
            .var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", "/v2/creds")
            .on("GET", "http://169.254.170.2/v2/creds", Ok((200, ROLE_JSON)));
        // CLI: env keys first
        assert_eq!(chain(&all, true).unwrap().source, Source::Env);
        // extension: env keys never count; the container comes before the instance role
        assert_eq!(chain(&all, false).unwrap().source, Source::Ecs);
        // no container: the instance role
        let mut no_ecs = all;
        no_ecs.vars.remove("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI");
        assert_eq!(chain(&no_ecs, false).unwrap().source, Source::InstanceRole);
        // half-set env keys are an error, not a fall-through
        let mut half = no_ecs;
        half.vars.remove("AWS_SECRET_ACCESS_KEY");
        assert!(chain(&half, true).unwrap_err().contains("must both be set"));
        // a configured container endpoint that fails is an error (no silent fall-through to the instance role)
        let bad = imds_ok(Fake::default()).var("AWS_CONTAINER_CREDENTIALS_FULL_URI", "http://127.0.0.1:9/creds");
        assert!(chain(&bad, false).unwrap_err().starts_with("s3 credentials (container credentials): no answer"));
        // web identity comes before the container
        let mut wi = imds_ok(Fake::default())
            .var("AWS_WEB_IDENTITY_TOKEN_FILE", "/t")
            .var("AWS_ROLE_ARN", "arn:aws:iam::1:role/r")
            .var("AWS_REGION", "eu-west-1")
            .var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", "/v2/creds")
            .on("POST", "https://sts.eu-west-1.amazonaws.com/", Ok((200, "<AssumeRoleWithWebIdentityResponse><AssumeRoleWithWebIdentityResult>\
                <Credentials><AccessKeyId>ASIAWI</AccessKeyId><SecretAccessKey>s</SecretAccessKey><SessionToken>t</SessionToken>\
                <Expiration>2026-10-03T16:00:00Z</Expiration></Credentials></AssumeRoleWithWebIdentityResult></AssumeRoleWithWebIdentityResponse>")));
        wi.files.insert("/t".into(), "jwt\n".into());
        let c = chain(&wi, false).unwrap();
        assert_eq!((c.source, c.s3().access_key.as_deref()), (Source::WebIdentity, Some("ASIAWI")));
    }

    #[test]
    fn container_authorization_header() {
        let mut f = Fake::default()
            .var("AWS_CONTAINER_CREDENTIALS_FULL_URI", "http://169.254.170.23/v1/credentials")
            .var("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE", "/var/run/secrets/token")
            .on("GET", "http://169.254.170.23/v1/credentials", Ok((200, ROLE_JSON)));
        f.files.insert("/var/run/secrets/token".into(), "podtoken".into());
        assert_eq!(chain(&f, false).unwrap().source, Source::Ecs);
        assert_eq!(f.calls.borrow()[0], "GET http://169.254.170.23/v1/credentials Authorization");
    }

    fn at(s: &str) -> DateTime<Utc> {
        rfc3339(s).unwrap()
    }
    fn temp(key: &str, exp: &str) -> Creds {
        Creds { source: Source::InstanceRole, detail: String::new(), access_key: key.into(), secret_key: "s".into(), token: Some("t".into()), expires: Some(at(exp)) }
    }

    #[test]
    fn refresh_timing() {
        let c = temp("A", "2026-10-03T12:00:00Z");
        assert!(!due(&c, at("2026-10-03T11:54:59Z")));
        assert!(due(&c, at("2026-10-03T11:55:00Z")), "5 minutes early");
        assert!(!due(&Creds::keys("a", "b"), at("2100-01-01T00:00:00Z")), "static keys never");

        let mut cache = Cache::default();
        // first use fetches; later uses before the window do not
        assert_eq!(cache.get(at("2026-10-03T11:00:00Z"), || Ok(temp("A", "2026-10-03T12:00:00Z"))).unwrap().access_key, "A");
        let calls = std::cell::Cell::new(0);
        let r = cache.get(at("2026-10-03T11:50:00Z"), || {
            calls.set(calls.get() + 1);
            Ok(temp("X", "2026-10-03T13:00:00Z"))
        });
        assert_eq!((r.unwrap().access_key.as_str(), calls.get()), ("A", 0));
        // in the window: fetched, new keys
        let r = cache.get(at("2026-10-03T11:55:01Z"), || Ok(temp("B", "2026-10-03T13:00:00Z")));
        assert_eq!(r.unwrap().access_key, "B");
        assert!(cache.issued("A") && cache.issued("B"));
        // the service still hands out the same (due) ones: ask again only after RETRY_SECS
        let mut c2 = Cache::default();
        c2.get(at("2026-10-03T11:56:00Z"), || Ok(temp("A", "2026-10-03T12:00:00Z"))).unwrap();
        let calls = std::cell::Cell::new(0);
        let mut ask = |t: &str| {
            c2.get(at(t), || {
                calls.set(calls.get() + 1);
                Ok(temp("A", "2026-10-03T12:00:00Z"))
            })
            .unwrap()
        };
        ask("2026-10-03T11:56:05Z");
        assert_eq!(calls.get(), 0, "within RETRY_SECS");
        ask("2026-10-03T11:56:10Z");
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn failed_refresh_keeps_valid_credentials_then_errors() {
        let mut cache = Cache::default();
        cache.get(at("2026-10-03T11:00:00Z"), || Ok(temp("A", "2026-10-03T12:00:00Z"))).unwrap();
        // refresh fails inside the window: the old ones still work
        let r = cache.get(at("2026-10-03T11:56:00Z"), || Err("s3 credentials: imds down".into()));
        assert_eq!(r.unwrap().access_key, "A");
        // after they expired: the error
        let r = cache.get(at("2026-10-03T12:00:01Z"), || Err("s3 credentials: imds down".into()));
        assert_eq!(r.unwrap_err(), "s3 credentials: imds down");
        // and no new attempt within RETRY_SECS
        let r = cache.get(at("2026-10-03T12:00:05Z"), || panic!("must not fetch"));
        assert!(r.is_err());
        let r = cache.get(at("2026-10-03T12:00:12Z"), || Ok(temp("C", "2026-10-03T13:00:00Z")));
        assert_eq!(r.unwrap().access_key, "C");
    }

    #[test]
    fn refused_by_s3_refetches_once() {
        let mut cache = Cache::default();
        cache.get(at("2026-10-03T11:00:00Z"), || Ok(temp("A", "2026-10-03T12:00:00Z"))).unwrap();
        cache.mark_stale();
        let r = cache.get(at("2026-10-03T11:00:01Z"), || Ok(temp("B", "2026-10-03T13:00:00Z")));
        assert_eq!(r.unwrap().access_key, "B");
        // a 403 that new credentials did not cure (e.g. a missing permission) does not hammer the service
        cache.mark_stale();
        cache.get(at("2026-10-03T11:00:02Z"), || Ok(temp("B", "2026-10-03T13:00:00Z"))).unwrap();
        cache.mark_stale();
        let r = cache.get(at("2026-10-03T11:00:03Z"), || panic!("must not fetch within RETRY_SECS"));
        assert_eq!(r.unwrap().access_key, "B");
        // static keys are never marked stale
        let mut s = Cache::default();
        s.get(at("2026-10-03T11:00:00Z"), || Ok(Creds::keys("K", "S"))).unwrap();
        s.mark_stale();
        s.get(at("2026-10-03T11:00:01Z"), || panic!("static keys are not refetched")).unwrap();
    }

    #[test]
    fn form_encoding() {
        assert_eq!(form_encode("arn:aws:iam::1:role/r a+b"), "arn%3Aaws%3Aiam%3A%3A1%3Arole%2Fr%20a%2Bb");
        assert_eq!(tag("<a><Code>Expired</Code></a>", "Code").as_deref(), Some("Expired"));
    }
}
