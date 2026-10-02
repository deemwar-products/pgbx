//! `pgbx serve` — the local web app served by the pgbx binary (React front end in web/, built to web/dist and
//! embedded here at compile time; `cargo build` never needs node).
//!
//! Same tiny std::net HTTP/1.1 server as `pgbx ui` (one thread per connection, `Connection: close`), plus:
//!   - binds 127.0.0.1 on a random free port by default (`--listen` overrides), prints the URL, opens the browser
//!     unless `--no-open`;
//!   - a per-run token: every /api/ call must carry it (`X-Pgbx-Token`), else 401. The browser gets it in the URL
//!     fragment, which is never sent to the server or written to a log;
//!   - the foreign-Host 403 of `pgbx ui` (DNS rebinding);
//!   - one kept-open, read-only admin connection per profile; a connection switcher over `pgbx profile list`;
//!     a profile with an adapter keeps ONE adapter running for the whole serve run (started on first use,
//!     stopped on exit / Ctrl-C);
//!   - reads go through the read-only functions of ui.rs; `pgbx query` through the same guard as the CLI;
//!   - actions are OFF unless `--allow-safe`, and then only the safe tier (backup now, verify now, restore into a
//!     NEW database, cancel a queued job), through the CLI's own code paths. Never guarded or destructive ones.

use crate::{client_only, conn, memories, profile, query, rows, ui, Args, Ctx, Level};
use postgres::Client;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const DEFAULT_LISTEN: &str = "127.0.0.1:0";
pub const TOKEN_HEADER: &str = "x-pgbx-token";
const MAX_BODY: usize = 1 << 20;
const MAX_ROWS: usize = 10_000;

/// The built web app (web/dist), embedded at compile time. Vite writes fixed names (web/vite.config.ts).
const ASSETS: &[(&str, &str, &[u8])] = &[
    ("/index.html", "text/html; charset=utf-8", include_bytes!("../../web/dist/index.html")),
    ("/assets/index.js", "text/javascript; charset=utf-8", include_bytes!("../../web/dist/assets/index.js")),
    ("/assets/index.css", "text/css; charset=utf-8", include_bytes!("../../web/dist/assets/index.css")),
];

/// The safe-tier actions the UI may trigger with --allow-safe: (name, CLI command it runs).
pub const ACTIONS: &[(&str, &str)] = &[("backup", "now"), ("verify", "verify"), ("restore", "db-restore"), ("cancel", "jobs cancel")];

// ---------------------------------------------------------------- routing (pure, unit-tested)

#[derive(Debug, PartialEq)]
pub enum Route {
    Asset(&'static str),
    Session,
    Overview,
    Queue,
    Load,
    Health,
    Db(String),
    Suggest(String),
    Memory(String),
    Query,
    SaveMemory(String),
    Action(String),
    NotFound,
    MethodNotAllowed,
}

impl Route {
    /// Every /api/ route needs the token; the static app does not (it holds no data).
    pub fn needs_token(&self) -> bool {
        !matches!(self, Route::Asset(_) | Route::NotFound | Route::MethodNotAllowed)
    }
}

fn asset(path: &str) -> Option<&'static str> {
    let p = if path == "/" { "/index.html" } else { path };
    ASSETS.iter().find(|(n, _, _)| *n == p).map(|(n, _, _)| *n)
}

/// `/api/<prefix>/<name>` -> the decoded name (one path segment, non-empty).
fn named(path: &str, prefix: &str) -> Option<String> {
    let n = ui::pct_decode(path.strip_prefix(prefix)?)?;
    (!n.is_empty() && !n.contains('/')).then_some(n)
}

pub fn route(method: &str, target: &str) -> Route {
    let path = target.split_once('?').map_or(target, |(p, _)| p);
    let is_api = path.starts_with("/api/");
    match method {
        "GET" => {}
        "POST" if is_api => {}
        _ => return Route::MethodNotAllowed,
    }
    if method == "POST" {
        return match path {
            "/api/query" => Route::Query,
            p => match (named(p, "/api/memory/"), named(p, "/api/action/")) {
                (Some(db), _) => Route::SaveMemory(db),
                (_, Some(a)) if ACTIONS.iter().any(|(n, _)| *n == a) => Route::Action(a),
                _ => Route::NotFound,
            },
        };
    }
    match path {
        "/api/session" => Route::Session,
        "/api/overview" => Route::Overview,
        "/api/queue" => Route::Queue,
        "/api/load" => Route::Load,
        "/api/health" => Route::Health,
        "/api/query" => Route::MethodNotAllowed,
        p if p.starts_with("/api/action/") => Route::MethodNotAllowed,
        p => {
            if let Some(n) = named(p, "/api/db/") {
                Route::Db(n)
            } else if let Some(n) = named(p, "/api/suggest/") {
                Route::Suggest(n)
            } else if let Some(n) = named(p, "/api/memory/") {
                Route::Memory(n)
            } else if let Some(a) = asset(p) {
                Route::Asset(a)
            } else {
                Route::NotFound
            }
        }
    }
}

/// Constant-time compare of the presented token with this run's token.
pub fn token_ok(given: Option<&str>, token: &str) -> bool {
    let Some(g) = given else { return false };
    let (a, b) = (g.trim().as_bytes(), token.as_bytes());
    if a.len() != b.len() || b.is_empty() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// 32 hex chars (128 bits) from the OS: /dev/urandom where there is one, else std's per-process random hash keys.
pub fn new_token() -> String {
    let mut b = [0u8; 16];
    let urandom = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).is_ok();
    if !urandom {
        use std::hash::{BuildHasher, Hasher};
        for (i, chunk) in b.chunks_mut(8).enumerate() {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_usize(i);
            h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
            chunk.copy_from_slice(&h.finish().to_le_bytes());
        }
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Warning printed when serve is reachable from other machines.
pub fn bind_warning(addr: &SocketAddr) -> Option<String> {
    (!addr.ip().is_loopback()).then(|| {
        format!(
            "WARNING: pgbx serve is listening on {addr}, a NON-LOOPBACK address. Anyone who can reach it and has the \
             token sees every database's backups and can run read-only queries. Prefer 127.0.0.1 plus an SSH tunnel."
        )
    })
}

// ---------------------------------------------------------------- sessions (one per profile)

struct Session {
    cx: Ctx,
    /// the kept-open, read-only admin connection
    admin: Mutex<Option<Client>>,
}

impl Session {
    fn new(a: Args, conn: Arc<conn::Conn>) -> Session {
        let mut cx = Ctx::new(a, conn);
        let _ = cx.admin_db(); // resolves pgbx.admin_db once (and starts the adapter when the profile has one)
        Session { cx, admin: Mutex::new(None) }
    }

    /// Run `f` on the kept admin connection; reconnect once when it was closed under us.
    fn with_admin<T>(&self, f: impl Fn(&mut Client) -> Result<T, String>) -> Result<T, String> {
        let mut g = self.admin.lock().unwrap_or_else(|e| e.into_inner());
        for attempt in 0..2 {
            if g.as_ref().is_none_or(|c| c.is_closed()) {
                *g = Some(ui::admin(&self.cx)?);
            }
            let c = g.as_mut().expect("connected above");
            match f(c) {
                Err(e) if attempt == 0 && c.is_closed() => {
                    *g = None;
                    let _ = e;
                }
                r => return r,
            }
        }
        Err("lost the connection to Postgres".into())
    }

    /// A Ctx for one CLI code path: this connection's flags plus `extra`, sharing the connection (and its adapter).
    fn ctx_for(&self, cmd: &str, pos: Vec<String>, extra: &[(&str, &str)]) -> Ctx {
        let mut flags: HashMap<String, String> = self.cx.a.flags.iter()
            .filter(|(k, _)| k.as_str() == "profile" || profile::FLAG_KEYS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone())).collect();
        for (k, v) in extra {
            flags.insert(k.to_string(), v.to_string());
        }
        Ctx { a: Args { cmd: cmd.into(), pos, flags }, admin_db: self.cx.admin_db.clone(), conn: Arc::clone(&self.cx.conn) }
    }

    /// The memory folder name for this connection: the profile, else the host (as `pgbx memories path`).
    fn connection(&self) -> String {
        self.cx.conn.memory_name()
    }
}

struct App {
    token: String,
    allow_safe: bool,
    loopback: bool,
    /// the session key of the connection serve was started with ("" = flags/env, no profile)
    start: String,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    warnings: Vec<String>,
}

impl App {
    fn session(&self, profile: Option<&str>) -> Result<Arc<Session>, String> {
        let key = profile.filter(|p| !p.is_empty()).unwrap_or(&self.start).to_string();
        if let Some(s) = self.sessions.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
            return Ok(Arc::clone(s));
        }
        profile::check_name(&key)?;
        let (a, c) = profile::args_for(&key, &profile::load_sys()?)?;
        let s = Arc::new(Session::new(a, Arc::new(c)));
        self.sessions.lock().unwrap_or_else(|e| e.into_inner()).entry(key).or_insert_with(|| Arc::clone(&s));
        Ok(s)
    }
}

// ---------------------------------------------------------------- api

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|s| !s.is_empty())
}

fn api_session(app: &App) -> Value {
    let (profiles, default, perr) = match profile::load_sys() {
        Ok(st) => {
            let p: Vec<Value> = st.profiles.iter().map(|(n, v)| json!({
                "name": n, "default": st.default.as_deref() == Some(n.as_str()),
                "adapter": v.get("adapter"), "url": v.get("url").and_then(|u| u.as_str()).map(conn::redact),
            })).collect();
            (p, st.default, Value::Null)
        }
        Err(e) => (vec![], None, json!(e)),
    };
    json!({"ok": true, "version": crate::skill::VERSION, "allow_safe": app.allow_safe,
           "safety": if app.allow_safe { "safe" } else { "read-only" },
           "start": app.start, "profiles": profiles, "default": default, "profiles_error": perr,
           "memory": memories::enabled(&env), "warnings": app.warnings,
           "actions": if app.allow_safe { json!(ACTIONS.iter().map(|(n, _)| n).collect::<Vec<_>>()) } else { json!([]) }})
}

fn api_overview(s: &Session) -> Result<Value, String> {
    s.with_admin(|c| {
        match client_only::ext(c)? {
            client_only::Ext::Here => {
                let mut v = ui::overview_with(c)?;
                v["backups"] = json!("on");
                v["windows"] = json!(rows(c, "SELECT database, window_cron, window_score, current_score, window_confidence
                                             FROM pgbx.server_overview ORDER BY database", &[]).unwrap_or_default());
                Ok(v)
            }
            client_only::Ext::Absent => {
                let who = crate::one(c, "SELECT current_user AS role, current_setting('default_transaction_read_only') AS read_only,
                                               now() AS server_time, current_setting('server_version') AS server_version", &[])?;
                let dbs = rows(c, "SELECT datname AS database, pg_size_pretty(pg_database_size(datname)) AS size
                                   FROM pg_database WHERE datallowconn AND NOT datistemplate ORDER BY 1", &[])?;
                Ok(json!({"ok": true, "backups": "off", "info": client_only::OFF, "next_steps": [client_only::turn_on()],
                          "connection": who, "databases": dbs}))
            }
            client_only::Ext::NotInDb => Err(client_only::status_without(&client_only::Ext::NotInDb, "the admin database")
                .and_then(|v| v["error"].as_str().map(String::from)).unwrap_or_default()),
        }
    })
}

fn api_db(s: &Session, name: &str) -> Result<Value, String> {
    s.with_admin(|c| ui::db_with(c, &s.cx, name)).map_err(|e| client_only::friendly("serve", e))
}

fn api_suggest(s: &Session, name: &str) -> Result<Value, String> {
    let mut c = ui::ro_connect(&s.cx, name)?;
    Ok(json!({"ok": true, "database": name, "suggestion": crate::one(&mut c, "SELECT * FROM pgbx.suggest_window()", &[])?,
              "cli": format!("pgbx schedule suggest --db {name} --apply")}))
}

fn api_health(s: &Session) -> Result<Value, String> {
    crate::cmd_doctor(&mut s.ctx_for("doctor", vec![], &[]))
}

fn memory_file(s: &Session, db: &str) -> Result<std::path::PathBuf, String> {
    if !memories::enabled(&env) {
        return Err("memory is off (PGBX_MEMORY=off)".into());
    }
    memories::memories_file(&env, &s.connection(), db)
}

fn api_memory(s: &Session, db: &str) -> Result<Value, String> {
    if !memories::enabled(&env) {
        return Ok(json!({"ok": true, "enabled": false, "questions": []}));
    }
    let f = memory_file(s, db)?;
    let text = std::fs::read_to_string(&f).unwrap_or_default();
    Ok(json!({"ok": true, "enabled": true, "path": f.display().to_string(), "exists": f.is_file(),
              "questions": memories::questions(&text)}))
}

fn api_save_memory(s: &Session, db: &str, b: &Value) -> Result<Value, String> {
    let f = memory_file(s, db)?;
    let str_of = |k: &str| b[k].as_str().unwrap_or("").to_string();
    let block = memories::append_question(&f, &str_of("name"), &str_of("note"), &str_of("sql"))?;
    Ok(json!({"ok": true, "path": f.display().to_string(), "appended": block,
              "message": format!("Saved '{}' to {}.", str_of("name").trim(), f.display())}))
}

fn api_query(s: &Session, b: &Value) -> Result<Value, String> {
    let db = b["db"].as_str().filter(|d| !d.is_empty()).ok_or("pick a database")?;
    let sql = b["sql"].as_str().ok_or("no SQL")?;
    let max = b["max_rows"].as_u64().map(|n| n as usize).unwrap_or(1000).min(MAX_ROWS);
    let timeout = query::parse_duration(b["timeout"].as_str().unwrap_or("30s"))?;
    let t0 = std::time::Instant::now();
    let mut v = query::run_sql(&s.cx, db, sql, max, timeout)?;
    v["elapsed_ms"] = json!(t0.elapsed().as_millis() as u64);
    Ok(v)
}

/// (command, positional args, flags) of the CLI command a UI action runs.
pub type ActionArgs = (String, Vec<String>, Vec<(String, String)>);

/// The CLI command (args) a UI action maps to, checked against the CLI's own safety level.
pub fn action_args(kind: &str, b: &Value) -> Result<ActionArgs, String> {
    let field = |k: &str| b[k].as_str().map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    let db = field("db").ok_or("--db: pick a database")?;
    let mut flags = vec![("db".to_string(), db)];
    let (cmd, pos) = match kind {
        "backup" => ("now", vec![]),
        "verify" => ("verify", vec![]),
        "restore" => {
            flags.push(("into".into(), field("into").ok_or("--into NEWDB: name the NEW database")?));
            if let Some(t) = field("time") {
                crate::check_time(&t)?;
                flags.push(("time".into(), t));
            }
            ("db-restore", vec![])
        }
        "cancel" => {
            let id = b["job_id"].as_i64().ok_or("job_id is required")?;
            flags.push(("yes".into(), String::new())); // the UI's confirm dialog is the --yes
            ("jobs", vec!["cancel".to_string(), id.to_string()])
        }
        x => return Err(format!("unknown action '{x}'")),
    };
    let a = Args { cmd: cmd.into(), pos: pos.clone(), flags: flags.iter().cloned().collect() };
    // cancelling is guarded in the CLI because it can stop a RUNNING job; the UI only cancels queued ones
    if kind != "cancel" && crate::level(cmd, &a) != Level::Safe {
        return Err(format!("refusing: '{kind}' is not a safe-tier action"));
    }
    Ok((cmd.into(), pos, flags))
}

fn api_action(s: &Session, kind: &str, b: &Value) -> Result<Value, String> {
    let (cmd, pos, flags) = action_args(kind, b)?;
    let extra: Vec<(&str, &str)> = flags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let mut cx = s.ctx_for(&cmd, pos, &extra);
    let db = cx.db();
    let r = match kind {
        "backup" => crate::cmd_now(&mut cx),
        "verify" => crate::cmd_verify(&mut cx),
        "restore" => crate::cmd_db_restore(&mut cx),
        _ => {
            // queued only, atomically: the row lock holds off the worker, whose start re-checks state='queued',
            // so a job that would start right now is either cancelled before it starts or refused here
            let id = b["job_id"].as_i64().unwrap_or(0);
            let mut c = cx.connect(&db)?;
            let mut t = c.transaction().map_err(crate::pe)?;
            let st: Option<String> = t
                .query_opt("SELECT state FROM pgbx.history WHERE id = $1 FOR UPDATE", &[&id])
                .map_err(crate::pe)?
                .map(|r| r.get(0));
            match st.as_deref() {
                Some("queued") => {
                    let msg: String = t.query_one("SELECT pgbx.cancel($1)", &[&id]).map_err(crate::pe)?.get(0);
                    t.commit().map_err(crate::pe)?;
                    Ok(json!({"ok": true, "database": db, "job_id": id, "message": msg}))
                }
                Some(x) => Err(format!("job {id} in {db} is {x}: the UI only cancels queued jobs; \
                                        a running one: pgbx jobs cancel {id} --db {db} --yes")),
                None => Err(format!("no job {id} in {db}")),
            }
        }
    };
    let mut v = r.map_err(|e| client_only::friendly(&cmd, e))?;
    v["action"] = json!(kind);
    v["cli"] = json!(cli_line(&cmd, &s.cx.a, &flags, b));
    Ok(v)
}

/// The equivalent CLI command, for the record.
fn cli_line(cmd: &str, a: &Args, flags: &[(String, String)], b: &Value) -> String {
    let mut s = format!("pgbx {cmd}");
    if cmd == "jobs" {
        s.push_str(&format!(" cancel {}", b["job_id"]));
    }
    for (k, v) in flags {
        s.push_str(&if v.is_empty() { format!(" --{k}") } else if v.contains([' ', '\'', '"']) { format!(" --{k} '{v}'") } else { format!(" --{k} {v}") });
    }
    if let Some(p) = a.get("profile") {
        s.push_str(&format!(" --profile {p}"));
    }
    s
}

// ---------------------------------------------------------------- http

fn respond(s: &mut TcpStream, code: u16, ctype: &str, body: &[u8]) {
    let reason = match code {
        200 => "OK", 400 => "Bad Request", 401 => "Unauthorized", 403 => "Forbidden", 404 => "Not Found",
        405 => "Method Not Allowed", 413 => "Payload Too Large", 502 => "Bad Gateway", _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\nReferrer-Policy: no-referrer\r\n\
         Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'\r\n\r\n",
        body.len()
    );
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(body);
}

fn json_err(s: &mut TcpStream, code: u16, e: &str) {
    respond(s, code, "application/json", json!({"ok": false, "error": crate::vars::scrub(e)}).to_string().as_bytes());
}

fn json_resp(s: &mut TcpStream, r: Result<Value, String>) {
    match r {
        Ok(v) => respond(s, 200, "application/json", crate::vars::scrub_value(&v).to_string().as_bytes()),
        Err(e) => json_err(s, 502, &e),
    }
}

pub struct Request {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, k: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str())
    }
    pub fn param(&self, k: &str) -> Option<String> {
        self.target.split_once('?').and_then(|(_, q)| ui::query_param(q, k)).and_then(|v| ui::pct_decode(&v.replace('+', " ")))
    }
}

/// Parse one request (head, then a Content-Length body up to MAX_BODY). Err(code) for a bad or too large one.
pub fn read_request(s: &mut impl Read) -> Result<Request, u16> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > 16 * 1024 {
            return Err(400);
        }
        let n = s.read(&mut chunk).map_err(|_| 400u16)?;
        if n == 0 {
            return Err(400);
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = std::str::from_utf8(&buf[..end]).map_err(|_| 400u16)?;
    let mut lines = head.split("\r\n");
    let mut rl = lines.next().unwrap_or("").split_whitespace();
    let (method, target) = (rl.next().unwrap_or("").to_string(), rl.next().unwrap_or("").to_string());
    let headers: Vec<(String, String)> =
        lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect();
    let len: usize = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .map(|(_, v)| v.parse().map_err(|_| 400u16)).transpose()?.unwrap_or(0);
    if len > MAX_BODY {
        return Err(413);
    }
    let mut body = buf[end + 4..].to_vec();
    while body.len() < len {
        let n = s.read(&mut chunk).map_err(|_| 400u16)?;
        if n == 0 {
            return Err(400);
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    Ok(Request { method, target, headers, body })
}

/// The gate in front of every request: Some((code, error)) to refuse it, None to serve it.
pub fn gate(r: &Request, route: &Route, token: &str, loopback: bool, allow_safe: bool) -> Option<(u16, &'static str)> {
    if !ui::host_allowed(r.header("host"), loopback) {
        return Some((403, "forbidden host"));
    }
    if route.needs_token() && !token_ok(r.header(TOKEN_HEADER), token) {
        return Some((401, "missing or wrong token: open the URL pgbx serve printed (it carries the token)"));
    }
    if matches!(route, Route::Action(_)) && !allow_safe {
        return Some((403, "actions are off: restart with pgbx serve --allow-safe to back up, verify, restore into a NEW database or cancel a queued job"));
    }
    if r.method == "POST" && !r.header("content-type").is_some_and(|c| c.starts_with("application/json")) {
        return Some((400, "POST bodies are JSON (Content-Type: application/json)"));
    }
    None
}

fn handle(mut s: TcpStream, app: &App) {
    let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(60)));
    let r = match read_request(&mut s) {
        Ok(r) => r,
        Err(code) => return json_err(&mut s, code, "bad request"),
    };
    let rt = route(&r.method, &r.target);
    if let Some((code, e)) = gate(&r, &rt, &app.token, app.loopback, app.allow_safe) {
        return json_err(&mut s, code, e);
    }
    let body: Value = if r.body.is_empty() { Value::Null } else {
        match serde_json::from_slice(&r.body) {
            Ok(v) => v,
            Err(e) => return json_err(&mut s, 400, &format!("body is not JSON: {e}")),
        }
    };
    let sess = || app.session(r.param("profile").as_deref());
    match rt {
        Route::MethodNotAllowed => json_err(&mut s, 405, "method not allowed"),
        Route::NotFound => json_err(&mut s, 404, "not found"),
        Route::Asset(p) => {
            let (_, ct, b) = ASSETS.iter().find(|(n, _, _)| *n == p).expect("routed assets exist");
            respond(&mut s, 200, ct, b)
        }
        Route::Session => json_resp(&mut s, Ok(api_session(app))),
        Route::Overview => json_resp(&mut s, sess().and_then(|x| api_overview(&x))),
        Route::Queue => json_resp(&mut s, sess().and_then(|x| x.with_admin(ui::queue_with)).map_err(|e| client_only::friendly("jobs", e))),
        Route::Load => json_resp(&mut s, sess().and_then(|x| x.with_admin(ui::load_with)).map_err(|e| client_only::friendly("load", e))),
        Route::Health => json_resp(&mut s, sess().and_then(|x| api_health(&x))),
        Route::Db(n) => json_resp(&mut s, sess().and_then(|x| api_db(&x, &n))),
        Route::Suggest(n) => json_resp(&mut s, sess().and_then(|x| api_suggest(&x, &n)).map_err(|e| client_only::friendly("schedule", e))),
        Route::Memory(n) => json_resp(&mut s, sess().and_then(|x| api_memory(&x, &n))),
        Route::Query => json_resp(&mut s, sess().and_then(|x| api_query(&x, &body))),
        Route::SaveMemory(n) => json_resp(&mut s, sess().and_then(|x| api_save_memory(&x, &n, &body))),
        Route::Action(k) => json_resp(&mut s, sess().and_then(|x| api_action(&x, &k, &body))),
    }
}

fn open_browser(url: &str) {
    let r = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).spawn()
    } else if cfg!(windows) {
        std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(url).spawn()
    };
    if r.is_err() {
        eprintln!("pgbx serve: could not open a browser; open the URL above yourself");
    }
}

pub fn run(cx: &mut Ctx) -> Result<Value, String> {
    let listen = cx.a.get("listen").unwrap_or(DEFAULT_LISTEN).to_string();
    let addr: SocketAddr = listen.parse().map_err(|_| format!("--listen '{listen}' must be IP:PORT, e.g. 127.0.0.1:0 (0 = any free port)"))?;
    let allow_safe = cx.a.has("allow-safe");
    let start = cx.a.get("profile").unwrap_or("").to_string();
    let mut a = ui::clone_args(&cx.a);
    a.cmd = "serve".into();
    let first = Session::new(a, Arc::clone(&cx.conn));
    let mut warnings = vec![];
    match ui::check_role(&first.cx, false) {
        // with --allow-safe the role is meant to run backups; without it, say so like pgbx ui does
        Ok((_, Some(w))) if !allow_safe => warnings.push(w.replace("pgbx ui --user", "pgbx serve --user")),
        Ok(_) => {}
        // not a lasting warning: the page reconnects on every call and shows the error where it happens
        Err(e) => eprintln!("pgbx serve: cannot read the connection yet: {e}"),
    }
    warnings.extend(bind_warning(&addr));
    let listener = TcpListener::bind(addr).map_err(|e| format!("cannot listen on {addr}: {e}"))?;
    let local = listener.local_addr().map_err(|e| e.to_string())?;
    let token = new_token();
    let url = format!("http://{local}/#token={token}");
    let app = Arc::new(App {
        token, allow_safe, loopback: addr.ip().is_loopback(), start: start.clone(),
        sessions: Mutex::new(HashMap::from([(start, Arc::new(first))])), warnings: warnings.clone(),
    });
    for w in &warnings {
        eprintln!("pgbx serve: {w}");
    }
    let safety = if allow_safe { "safe" } else { "read-only" };
    if cx.a.has("json") {
        println!("{}", json!({"ok": true, "command": "serve", "safety": safety, "url": url, "listen": local.to_string(), "warnings": warnings}));
    } else {
        let acts = if allow_safe { "safe actions ON (each one asks to confirm)" } else { "read-only; actions off (--allow-safe)" };
        println!("pgbx serve: {url}\n  {acts}; the token in the link is for this run only; Ctrl-C to stop");
    }
    let _ = std::io::stdout().flush();
    if !cx.a.has("no-open") {
        open_browser(&url);
    }
    for s in listener.incoming().flatten() {
        let app = Arc::clone(&app);
        std::thread::spawn(move || handle(s, &app));
    }
    Ok(json!({"ok": true}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(raw: &str) -> Request {
        read_request(&mut raw.as_bytes()).unwrap()
    }

    #[test]
    fn routes() {
        assert_eq!(route("GET", "/"), Route::Asset("/index.html"));
        assert_eq!(route("GET", "/index.html"), Route::Asset("/index.html"));
        assert_eq!(route("GET", "/assets/index.js"), Route::Asset("/assets/index.js"));
        assert_eq!(route("GET", "/assets/index.css?v=1"), Route::Asset("/assets/index.css"));
        assert_eq!(route("GET", "/assets/../../etc/passwd"), Route::NotFound);
        assert_eq!(route("GET", "/api/session"), Route::Session);
        assert_eq!(route("GET", "/api/overview?profile=prod"), Route::Overview);
        assert_eq!(route("GET", "/api/queue"), Route::Queue);
        assert_eq!(route("GET", "/api/load"), Route::Load);
        assert_eq!(route("GET", "/api/health"), Route::Health);
        assert_eq!(route("GET", "/api/db/my%20db"), Route::Db("my db".into()));
        assert_eq!(route("GET", "/api/db/a%2Fb"), Route::NotFound);
        assert_eq!(route("GET", "/api/db/"), Route::NotFound);
        assert_eq!(route("GET", "/api/suggest/shop"), Route::Suggest("shop".into()));
        assert_eq!(route("GET", "/api/memory/shop"), Route::Memory("shop".into()));
        assert_eq!(route("POST", "/api/memory/shop"), Route::SaveMemory("shop".into()));
        assert_eq!(route("POST", "/api/query"), Route::Query);
        assert_eq!(route("GET", "/api/query"), Route::MethodNotAllowed);
        for a in ["backup", "verify", "restore", "cancel"] {
            assert_eq!(route("POST", &format!("/api/action/{a}")), Route::Action(a.into()));
            assert_eq!(route("GET", &format!("/api/action/{a}")), Route::MethodNotAllowed);
        }
        for bad in ["pause", "retention", "drop", "setup", ""] {
            assert_eq!(route("POST", &format!("/api/action/{bad}")), Route::NotFound, "{bad}");
        }
        assert_eq!(route("POST", "/"), Route::MethodNotAllowed);
        assert_eq!(route("POST", "/api/overview"), Route::NotFound);
        for m in ["PUT", "DELETE", "PATCH", "HEAD", "OPTIONS", "get", ""] {
            assert_eq!(route(m, "/api/overview"), Route::MethodNotAllowed, "{m}");
        }
        assert_eq!(route("GET", "/api/nope"), Route::NotFound);
        assert!(!Route::Asset("/index.html").needs_token());
        assert!(Route::Session.needs_token() && Route::Query.needs_token() && Route::Action("backup".into()).needs_token());
    }

    #[test]
    fn token() {
        let t = new_token();
        assert_eq!(t.len(), 32);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(t, new_token());
        assert!(token_ok(Some(&t), &t));
        assert!(token_ok(Some(&format!(" {t} ")), &t));
        assert!(!token_ok(None, &t));
        assert!(!token_ok(Some(""), &t));
        assert!(!token_ok(Some(&t[..31]), &t));
        assert!(!token_ok(Some(&format!("{t}0")), &t));
        assert!(!token_ok(Some(""), ""));
    }

    #[test]
    fn gate_checks_host_token_and_actions() {
        let t = "abc";
        let get = |path: &str, host: &str, tok: &str| {
            req(&format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nX-Pgbx-Token: {tok}\r\n\r\n"))
        };
        let g = |r: &Request, safe: bool| gate(r, &route(&r.method, &r.target), t, true, safe).map(|x| x.0);
        assert_eq!(g(&get("/api/overview", "127.0.0.1:9", "abc"), false), None);
        assert_eq!(g(&get("/api/overview", "localhost:9", "abc"), false), None);
        assert_eq!(g(&get("/api/overview", "127.0.0.1:9", "abd"), false), Some(401));
        assert_eq!(g(&req("GET /api/overview HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"), false), Some(401));
        assert_eq!(g(&get("/", "127.0.0.1:9", ""), false), None); // the app itself carries no data
        assert_eq!(g(&get("/api/overview", "evil.example", "abc"), false), Some(403));
        assert_eq!(g(&get("/", "evil.example", ""), false), Some(403));
        assert_eq!(g(&req("GET / HTTP/1.1\r\n\r\n"), false), Some(403));
        let post = |path: &str, tok: &str| req(&format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Pgbx-Token: {tok}\r\nContent-Type: application/json\r\nContent-Length: 12\r\n\r\n{{\"db\":\"shop\"}}"));
        assert_eq!(g(&post("/api/action/backup", "abc"), false), Some(403));
        assert_eq!(g(&post("/api/action/backup", "abc"), true), None);
        assert_eq!(g(&post("/api/action/backup", "nope"), true), Some(401));
        assert_eq!(g(&post("/api/query", "abc"), false), None); // read-only, allowed without --allow-safe
        let r = req("POST /api/query HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Pgbx-Token: abc\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\n{}");
        assert_eq!(g(&r, false), Some(400));
        // non-loopback listen: any Host, the token still required
        let r = get("/api/overview", "db.internal", "abc");
        assert_eq!(gate(&r, &route(&r.method, &r.target), t, false, false), None);
        let r = get("/api/overview", "db.internal", "x");
        assert_eq!(gate(&r, &route(&r.method, &r.target), t, false, false).map(|x| x.0), Some(401));
    }

    #[test]
    fn request_parsing() {
        let r = req("POST /api/query?profile=my%20prod HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nhello-extra");
        assert_eq!((r.method.as_str(), r.body.as_slice()), ("POST", &b"hello"[..]));
        assert_eq!(r.param("profile").as_deref(), Some("my prod"));
        assert_eq!(r.header("HOST"), Some("x"));
        let big = format!("POST /api/query HTTP/1.1\r\nContent-Length: {}\r\n\r\n", MAX_BODY + 1);
        assert_eq!(read_request(&mut big.as_bytes()).err(), Some(413));
        assert_eq!(read_request(&mut &b"GET / HTTP/1.1\r\nContent-Length: 9\r\n\r\nshort"[..]).err(), Some(400));
        assert_eq!(read_request(&mut &b"GET / HTTP/1.1\r\n"[..]).err(), Some(400));
    }

    #[test]
    fn actions_are_safe_tier_only() {
        let (cmd, pos, f) = action_args("backup", &json!({"db": "shop"})).unwrap();
        assert_eq!((cmd.as_str(), pos.len(), f[0].1.as_str()), ("now", 0, "shop"));
        assert_eq!(action_args("verify", &json!({"db": "shop"})).unwrap().0, "verify");
        let (cmd, _, f) = action_args("restore", &json!({"db": "shop", "into": "shop_copy", "time": "2026-01-31 14:00:00+00"})).unwrap();
        assert_eq!(cmd, "db-restore");
        assert!(f.contains(&("into".into(), "shop_copy".into())));
        assert!(action_args("restore", &json!({"db": "shop"})).unwrap_err().contains("--into"));
        assert!(action_args("restore", &json!({"db": "shop", "into": "x", "time": "yesterday"})).unwrap_err().contains("UTC offset"));
        let (cmd, pos, f) = action_args("cancel", &json!({"db": "shop", "job_id": 7})).unwrap();
        assert_eq!((cmd.as_str(), pos), ("jobs", vec!["cancel".to_string(), "7".to_string()]));
        assert!(f.iter().any(|(k, _)| k == "yes"));
        assert!(action_args("backup", &json!({})).is_err());
        assert!(action_args("pause", &json!({"db": "shop"})).is_err());
        assert!(ACTIONS.iter().all(|(n, _)| route("POST", &format!("/api/action/{n}")) == Route::Action(n.to_string())));
        let line = cli_line("db-restore", &Args::default(), &[("db".into(), "shop".into()), ("time".into(), "2026-01-31 14:00:00+00".into())], &Value::Null);
        assert_eq!(line, "pgbx db-restore --db shop --time '2026-01-31 14:00:00+00'");
    }

    #[test]
    fn bind_warns_off_loopback() {
        assert!(bind_warning(&"127.0.0.1:0".parse().unwrap()).is_none());
        assert!(bind_warning(&"0.0.0.0:8433".parse().unwrap()).unwrap().contains("NON-LOOPBACK"));
    }

    #[test]
    fn embedded_app_is_self_contained() {
        let html = std::str::from_utf8(ASSETS[0].2).unwrap();
        assert!(html.contains("/assets/index.js") && html.contains("/assets/index.css"), "{html}");
        for (name, _, body) in ASSETS {
            let t = String::from_utf8_lossy(body);
            for bad in ["<script src=\"http", "<link href=\"http", "fetch(\"http", "fetch('http", "sendBeacon"] {
                assert!(!t.contains(bad), "{name} must not contain {bad}");
            }
        }
        let js = String::from_utf8_lossy(ASSETS[1].2);
        assert!(js.contains("x-pgbx-token") || js.contains("X-Pgbx-Token"), "the app sends the token header");
    }
}
