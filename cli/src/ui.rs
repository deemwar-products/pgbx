//! `pgbx ui` — a READ-ONLY audit web UI served from the pgbx binary.
//!
//! Tiny HTTP/1.1 server on std::net (one thread per connection, `Connection: close`). The page and its
//! script are embedded at build time; no CDN, no external fetch. Only GET is served (anything else: 405).
//! Every database connection runs `SET default_transaction_read_only = on` before its first query, and
//! the UI warns (or with --strict refuses) when the role could change anything (pause, backup_now, ...).

use crate::{one, pe, rows, Args, Ctx};
use postgres::Client;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

pub const DEFAULT_LISTEN: &str = "127.0.0.1:8432";
pub const READ_ONLY_SQL: &str = "SET default_transaction_read_only = on";
const PAGE: &str = include_str!("ui.html");
const VERIFY_PREFIX: &str = "pgbx_verify_";
const TIMELINE_MAX_ROWS: usize = 5000;

/// Functions that would let the UI's role change something. Any of them executable => not a viewer.
pub const WRITE_FUNCTIONS: &[&str] = &[
    "pgbx.pause(text)",
    "pgbx.resume()",
    "pgbx.backup_now()",
    "pgbx.restore(text, timestamptz)",
    "pgbx.configure(text, int, int, bool, text)",
    "pgbx.download_url(bigint, interval)",
];

// ---------------------------------------------------------------- routing (pure, unit-tested)

#[derive(Debug, PartialEq)]
pub enum Route {
    Index,
    Overview,
    Db(String),
    Timeline { days: i32 },
    Health,
    NotFound,
    MethodNotAllowed,
}

pub fn route(method: &str, target: &str) -> Route {
    if method != "GET" {
        return Route::MethodNotAllowed;
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    match path {
        "/" | "/index.html" => Route::Index,
        "/api/overview" => Route::Overview,
        "/api/health" => Route::Health,
        "/api/timeline" => {
            let days = query_param(query, "days").and_then(|d| d.parse::<i32>().ok()).unwrap_or(30).clamp(1, 3650);
            Route::Timeline { days }
        }
        p => match p.strip_prefix("/api/db/").map(pct_decode) {
            Some(Some(n)) if !n.is_empty() && !n.contains('/') => Route::Db(n),
            _ => Route::NotFound,
        },
    }
}

fn query_param<'a>(q: &'a str, k: &str) -> Option<&'a str> {
    q.split('&').find_map(|kv| kv.split_once('=').filter(|(a, _)| *a == k).map(|(_, v)| v))
}

pub fn pct_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(h, 16).ok()?);
                i += 3;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Warning printed when the UI is reachable from other machines.
pub fn bind_warning(addr: &SocketAddr) -> Option<String> {
    (!addr.ip().is_loopback()).then(|| {
        format!(
            "WARNING: pgbx ui is listening on {addr}, a NON-LOOPBACK address. Anyone who can reach it sees every \
             database's backup history. Exposing this is your responsibility; it is read-only. Prefer 127.0.0.1 \
             plus an SSH tunnel or an authenticating reverse proxy."
        )
    })
}

/// DNS-rebinding guard: a loopback-bound UI only answers Host: localhost / 127.0.0.1 / [::1].
pub fn host_allowed(host: Option<&str>, loopback: bool) -> bool {
    if !loopback {
        return true;
    }
    let Some(h) = host else { return false };
    let h = h.trim();
    let name = if let Some(r) = h.strip_prefix('[') { r.split(']').next().unwrap_or("") } else { h.split(':').next().unwrap_or("") };
    matches!(name.to_ascii_lowercase().as_str(), "localhost" | "127.0.0.1" | "::1")
}

/// Decide whether the role may serve the UI. `can`: write functions it may execute.
/// Ok(Some(w)) = serve with a strong warning; Err = refuse (--strict).
pub fn privilege_verdict(role: &str, superuser: bool, can: &[String], strict: bool) -> Result<Option<String>, String> {
    if !superuser && can.is_empty() {
        return Ok(None);
    }
    let what = if superuser { "is a SUPERUSER".to_string() } else { format!("can execute {}", can.join(", ")) };
    let msg = format!(
        "role '{role}' {what}: it could change backups. The UI never calls these (every connection is \
         read-only and only GET is served), but serve it with a dedicated viewer login instead: \
         CREATE ROLE pgbx_ui LOGIN PASSWORD '...' IN ROLE pgbx_viewer; then pgbx ui --user pgbx_ui"
    );
    if strict { Err(format!("refusing (--strict): {msg}")) } else { Ok(Some(format!("WARNING: {msg}"))) }
}

// ---------------------------------------------------------------- database access (read-only)

pub fn ro_connect(cx: &Ctx, db: &str) -> Result<Client, String> {
    let mut c = cx.connect(db)?;
    c.batch_execute(READ_ONLY_SQL).map_err(pe)?;
    let v: String = c.query_one("SHOW default_transaction_read_only", &[]).map_err(pe)?.get(0);
    if v != "on" {
        return Err("could not make the connection read-only".into());
    }
    Ok(c)
}

fn admin(cx: &Ctx) -> Result<Client, String> {
    ro_connect(cx, cx.admin_db.as_deref().unwrap_or("postgres"))
}

fn api_overview(cx: &Ctx) -> Result<Value, String> {
    let mut c = admin(cx)?;
    let who = one(&mut c, "SELECT current_user AS role, current_setting('default_transaction_read_only') AS read_only, \
                            now() AS server_time, current_setting('pgbx.server_name', true) AS server_name", &[])?;
    Ok(json!({"ok": true, "connection": who, "databases": rows(&mut c, "SELECT * FROM pgbx.overview()", &[])?}))
}

/// History rows with a display kind (download_url / scope are recorded as 'config').
fn history_sql(where_: &str) -> String {
    format!(
        "SELECT current_database() AS database, id,
                CASE WHEN kind = 'config' AND params ? 'download_url' THEN 'download_url'
                     WHEN kind = 'config' AND (params ? 'include_data' OR params ? 'exclude_data') THEN 'scope'
                     ELSE kind END AS kind,
                trigger, state, coalesce(to_jsonb(h)->>'who', params->>'by', trigger) AS who,
                coalesce(finished, started, requested_at) AS at, requested_at, started, finished,
                s3_key, bytes, error, params
         FROM pgbx.history h {where_} ORDER BY coalesce(finished, started, requested_at) DESC, id DESC"
    )
}

fn api_db(cx: &Ctx, name: &str) -> Result<Value, String> {
    let mut a = admin(cx)?;
    let exists: bool = a.query_one("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1 AND datallowconn)", &[&name])
        .map_err(pe)?.get(0);
    if !exists {
        return Err(format!("no such database '{name}'"));
    }
    let mut c = ro_connect(cx, name)?;
    Ok(json!({
        "ok": true, "database": name,
        "status": one(&mut c, "SELECT * FROM pgbx.status()", &[])?,
        "backups": rows(&mut c, "SELECT id, taken_at, age::text, trigger, size, bytes, s3_key FROM pgbx.backups", &[])?,
        "history": rows(&mut c, &(history_sql("") + " LIMIT 200"), &[])?,
    }))
}

fn api_timeline(cx: &Ctx, days: i32) -> Result<Value, String> {
    let mut a = admin(cx)?;
    let dbs: Vec<String> = a
        .query("SELECT datname::text FROM pg_database WHERE datallowconn AND NOT datistemplate AND datname NOT LIKE $1 ORDER BY 1",
               &[&format!("{VERIFY_PREFIX}%")])
        .map_err(pe)?.iter().map(|r| r.get(0)).collect();
    let sql = history_sql("WHERE coalesce(finished, started, requested_at) > now() - make_interval(days => $1)");
    let (mut all, mut errors) = (vec![], vec![]);
    for db in &dbs {
        match ro_connect(cx, db).and_then(|mut c| rows(&mut c, &sql, &[&days])) {
            Ok(r) => all.extend(r),
            Err(e) => errors.push(json!({"database": db, "error": e})),
        }
    }
    all.sort_by(|x, y| y["at"].as_str().cmp(&x["at"].as_str()));
    let truncated = all.len() > TIMELINE_MAX_ROWS;
    all.truncate(TIMELINE_MAX_ROWS);
    Ok(json!({"ok": true, "days": days, "databases": dbs, "rows": all, "truncated": truncated, "errors": errors}))
}

fn api_health(cx: &Ctx) -> Result<Value, String> {
    let mut c = admin(cx)?;
    let checks = rows(&mut c, "SELECT * FROM pgbx.doctor()", &[])?;
    let healthy = checks.iter().all(|r| r["ok"] == true);
    Ok(json!({"ok": true, "healthy": healthy, "checks": checks}))
}

// ---------------------------------------------------------------- http

fn respond(s: &mut TcpStream, code: u16, ctype: &str, body: &[u8], extra: &str) {
    let reason = match code {
        200 => "OK", 400 => "Bad Request", 403 => "Forbidden", 404 => "Not Found", 405 => "Method Not Allowed",
        502 => "Bad Gateway", _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\nReferrer-Policy: no-referrer\r\n\
         Content-Security-Policy: default-src 'none'; script-src 'self' 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; img-src data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'\r\n{extra}\r\n",
        body.len()
    );
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(body);
}

fn json_resp(s: &mut TcpStream, r: Result<Value, String>) {
    let (code, v) = match r {
        Ok(v) => (200, v),
        Err(e) => (502, json!({"ok": false, "error": e})),
    };
    respond(s, code, "application/json", v.to_string().as_bytes(), "");
}

/// Read the request head (request line + headers). Bodies are never read: nothing here accepts one.
fn read_head(s: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = s.read(&mut chunk).ok()?;
        if n == 0 || buf.len() > 16 * 1024 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    String::from_utf8(buf).ok()
}

fn handle(mut s: TcpStream, cx: &Ctx, loopback: bool) {
    let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(30)));
    let Some(head) = read_head(&mut s) else { return };
    let mut lines = head.split("\r\n");
    let mut rl = lines.next().unwrap_or("").split_whitespace();
    let (method, target) = (rl.next().unwrap_or(""), rl.next().unwrap_or(""));
    let host = lines.filter_map(|l| l.split_once(':')).find(|(k, _)| k.trim().eq_ignore_ascii_case("host")).map(|(_, v)| v.trim());
    if !host_allowed(host, loopback) {
        return respond(&mut s, 403, "text/plain", b"forbidden host\n", "");
    }
    match route(method, target) {
        Route::MethodNotAllowed => respond(&mut s, 405, "application/json",
            br#"{"ok":false,"error":"read-only UI: only GET is allowed"}"#, "Allow: GET\r\n"),
        Route::NotFound => respond(&mut s, 404, "application/json", br#"{"ok":false,"error":"not found"}"#, ""),
        Route::Index => respond(&mut s, 200, "text/html; charset=utf-8", PAGE.as_bytes(), ""),
        Route::Overview => json_resp(&mut s, api_overview(cx)),
        Route::Db(n) => json_resp(&mut s, api_db(cx, &n)),
        Route::Timeline { days } => json_resp(&mut s, api_timeline(cx, days)),
        Route::Health => json_resp(&mut s, api_health(cx)),
    }
}

/// Privilege check at startup (admin database, read-only connection).
fn check_role(cx: &Ctx, strict: bool) -> Result<(String, Option<String>), String> {
    let mut c = admin(cx)?;
    let fns: Vec<String> = WRITE_FUNCTIONS.iter().map(|s| s.to_string()).collect();
    let r = c.query_one(
        "SELECT current_user::text, (SELECT rolsuper FROM pg_roles WHERE rolname = current_user),
                ARRAY(SELECT f FROM unnest($1::text[]) f
                      WHERE has_function_privilege(current_user, to_regprocedure(f), 'EXECUTE'))",
        &[&fns],
    ).map_err(pe)?;
    let (role, su, can): (String, bool, Vec<String>) = (r.get(0), r.get(1), r.get(2));
    let w = privilege_verdict(&role, su, &can, strict)?;
    Ok((role, w))
}

pub fn run(cx: &mut Ctx) -> Result<Value, String> {
    let listen = cx.a.get("listen").unwrap_or(DEFAULT_LISTEN).to_string();
    let addr: SocketAddr = listen.parse().map_err(|_| format!("--listen '{listen}' must be IP:PORT, e.g. 127.0.0.1:8432"))?;
    let admin_db = cx.admin_db();
    let ctx = Arc::new(Ctx { a: clone_args(&cx.a), admin_db: Some(admin_db), tunnel: Default::default() });
    let (role, priv_warn) = check_role(&ctx, cx.a.has("strict"))?;
    let mut warnings: Vec<String> = priv_warn.into_iter().collect();
    warnings.extend(bind_warning(&addr));
    let listener = TcpListener::bind(addr).map_err(|e| format!("cannot listen on {addr}: {e}"))?;
    let url = format!("http://{}/", listener.local_addr().map(|a| a.to_string()).unwrap_or(listen));
    for w in &warnings {
        eprintln!("pgbx ui: {w}");
    }
    let started = json!({"ok": true, "command": "ui", "safety": "read-only", "url": url, "role": role, "warnings": warnings});
    if cx.a.has("json") {
        println!("{started}");
    } else {
        println!("pgbx ui: read-only audit UI at {url} (role {role}; Ctrl-C to stop)");
    }
    let _ = std::io::stdout().flush();
    let loopback = addr.ip().is_loopback();
    for s in listener.incoming().flatten() {
        let ctx = Arc::clone(&ctx);
        std::thread::spawn(move || handle(s, &ctx, loopback));
    }
    Ok(json!({"ok": true}))
}

fn clone_args(a: &Args) -> Args {
    Args { cmd: a.cmd.clone(), pos: a.pos.clone(), flags: a.flags.clone() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_get_only() {
        assert_eq!(route("GET", "/"), Route::Index);
        assert_eq!(route("GET", "/api/overview"), Route::Overview);
        assert_eq!(route("GET", "/api/cluster"), Route::NotFound);
        assert_eq!(route("GET", "/api/health"), Route::Health);
        assert_eq!(route("GET", "/api/timeline"), Route::Timeline { days: 30 });
        assert_eq!(route("GET", "/api/timeline?days=7"), Route::Timeline { days: 7 });
        assert_eq!(route("GET", "/api/timeline?days=0"), Route::Timeline { days: 1 });
        assert_eq!(route("GET", "/api/timeline?days=x"), Route::Timeline { days: 30 });
        assert_eq!(route("GET", "/api/db/shop"), Route::Db("shop".into()));
        assert_eq!(route("GET", "/api/db/my%20db"), Route::Db("my db".into()));
        assert_eq!(route("GET", "/api/db/"), Route::NotFound);
        assert_eq!(route("GET", "/api/db/a%2Fb"), Route::NotFound);
        assert_eq!(route("GET", "/api/db/%zz"), Route::NotFound);
        assert_eq!(route("GET", "/etc/passwd"), Route::NotFound);
        for m in ["POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS", "get", ""] {
            assert_eq!(route(m, "/api/overview"), Route::MethodNotAllowed, "{m}");
            assert_eq!(route(m, "/nope"), Route::MethodNotAllowed, "{m}");
        }
    }

    #[test]
    fn bind_warns_off_loopback() {
        assert!(bind_warning(&"127.0.0.1:8432".parse().unwrap()).is_none());
        assert!(bind_warning(&"[::1]:8432".parse().unwrap()).is_none());
        let w = bind_warning(&"0.0.0.0:8432".parse().unwrap()).unwrap();
        assert!(w.contains("your responsibility") && w.contains("read-only"));
        assert!(bind_warning(&"10.1.2.3:80".parse().unwrap()).is_some());
    }

    #[test]
    fn host_guard() {
        assert!(host_allowed(Some("127.0.0.1:8432"), true));
        assert!(host_allowed(Some("localhost:8432"), true));
        assert!(host_allowed(Some("[::1]:8432"), true));
        assert!(!host_allowed(Some("evil.example:8432"), true));
        assert!(!host_allowed(None, true));
        assert!(host_allowed(Some("db.internal"), false));
    }

    #[test]
    fn read_only_enforcement() {
        assert_eq!(READ_ONLY_SQL, "SET default_transaction_read_only = on");
        assert_eq!(privilege_verdict("viewer", false, &[], true), Ok(None));
        let can = vec!["pgbx.backup_now()".to_string()];
        let w = privilege_verdict("ops", false, &can, false).unwrap().unwrap();
        assert!(w.contains("WARNING") && w.contains("backup_now") && w.contains("pgbx_viewer"));
        assert!(privilege_verdict("ops", false, &can, true).unwrap_err().contains("--strict"));
        assert!(privilege_verdict("postgres", true, &[], true).unwrap_err().contains("SUPERUSER"));
        assert!(WRITE_FUNCTIONS.contains(&"pgbx.pause(text)") && WRITE_FUNCTIONS.contains(&"pgbx.backup_now()"));
    }

    #[test]
    fn page_is_self_contained() {
        assert!(PAGE.contains("/api/overview") && PAGE.contains("/api/timeline") && PAGE.contains("/api/health"));
        for bad in ["http://", "https://", "<form", "method=\"post\"", "POST"] {
            assert!(!PAGE.contains(bad), "page must not contain {bad}");
        }
    }
}
