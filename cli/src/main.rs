//! pgbx — the pgbx command line. Uses SQL while Postgres is up; diagnoses it when it is down; restores
//! databases straight from S3 onto a new server (no extension needed there). Every command takes --json and
//! prints one JSON object.
//!
//! Safety levels (enforced here, see `Level`):
//!   read-only   status, list, backups, doctor, diagnose, logs, overview, "show" forms of policy commands
//!   safe        now, verify, db-restore (NEW db only, also --from-s3), resume, link, schedule, skill
//!   guarded     pause, lowering retention, narrowing scope, verify-schedule never — require --yes

mod diagnose;
mod memories;
mod policy;
mod profile;
mod query;
mod s3restore;
mod setup;
mod setup_client;
mod skill;
mod tunnel;
mod ui;

use postgres::types::ToSql;
use postgres::{Client, NoTls};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Unix: the Debian/RHEL socket dir. Windows has no unix socket default, so TCP to localhost.
#[cfg(unix)]
const DEFAULT_HOST: &str = "/var/run/postgresql";
#[cfg(not(unix))]
const DEFAULT_HOST: &str = "localhost";

// ---------------------------------------------------------------- arguments

const BOOL_FLAGS: &[&str] = &["json", "wait", "from-s3", "help", "yes", "reset", "no-codex", "version", "strict", "all", "no-skill", "overwrite"];
const VALUE_FLAGS: &[&str] = &[
    "db", "into", "time", "backup", "pgdata", "host", "port", "user", "admin-db", "timeout", "lines", "reason", "max-backups",
    "max-days", "include", "exclude", "backup-id", "expires", "log", "s3-endpoint", "s3-bucket", "s3-region", "server-name",
    "credentials-file", "listen", "access-key-env", "secret-key-env", "pg-conf", "profile", "ssh", "ssh-port",
    "ssh-jump", "tunnel-idle", "max-rows", "serve", "as",
];
const COMMANDS: &[&str] = &[
    "status", "list", "backups", "now", "verify", "db-restore", "doctor", "logs", "help", "schedule", "retention",
    "pause", "resume", "scope", "verify-schedule", "link", "overview", "skill", "diagnose", "ui", "setup", "profile", "query", "tunnel", "memories",
];

#[derive(Debug, Default, PartialEq)]
pub struct Args {
    pub cmd: String,
    pub pos: Vec<String>,
    pub flags: HashMap<String, String>,
}

impl Args {
    fn has(&self, k: &str) -> bool {
        self.flags.contains_key(k)
    }
    fn get(&self, k: &str) -> Option<&str> {
        self.flags.get(k).map(|s| s.as_str())
    }
}

pub fn parse_args<I: IntoIterator<Item = String>>(it: I) -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = it.into_iter();
    while let Some(s) = it.next() {
        if let Some(f) = s.strip_prefix("--") {
            let (name, inline) = match f.split_once('=') {
                Some((n, v)) => (n.to_string(), Some(v.to_string())),
                None => (f.to_string(), None),
            };
            if BOOL_FLAGS.contains(&name.as_str()) {
                if inline.is_some() {
                    return Err(format!("--{name} takes no value"));
                }
                a.flags.insert(name, String::new());
            } else if VALUE_FLAGS.contains(&name.as_str()) {
                let v = match inline {
                    Some(v) => v,
                    None => it.next().ok_or(format!("--{name} needs a value"))?,
                };
                a.flags.insert(name, v);
            } else {
                return Err(format!("unknown option --{name} (see pgbx help)"));
            }
        } else if s == "-h" {
            a.flags.insert("help".into(), String::new());
        } else if a.cmd.is_empty() {
            if !COMMANDS.contains(&s.as_str()) {
                return Err(format!("unknown command '{s}' (see pgbx help)"));
            }
            a.cmd = s;
        } else {
            a.pos.push(s);
        }
    }
    if a.cmd.is_empty() {
        a.cmd = "help".into();
    }
    Ok(a)
}

const HELP: &str = "pgbx — per-database Postgres backups to S3

read-only:
  pgbx status   [--db X]                     backup state of one database
  pgbx list     [--db X]                     that database's backups
  pgbx doctor                                health checks with a fix for each problem (runs diagnose when PG is down)
  pgbx diagnose [--log FILE] [--pgdata DIR]  why Postgres is down / disk full / pg_wal growing: cause, evidence,
                                             and steps tagged readonly/safe/guarded/destructive (never run by pgbx)
  pgbx logs     [--lines N]                  recent failed jobs
  pgbx ui       [--listen 127.0.0.1:8432] [--strict]
                                             read-only audit web UI (overview, 30-day timeline, health);
                                             --strict refuses a role that could change backups
safe:
  pgbx now      [--db X] [--wait]            queue a backup
  pgbx verify   [--db X] [--wait]            queue a restore test
  pgbx db-restore --db X --into NEWDB [--time TS] [--wait]
                                             restore into a NEW database on this server (via the extension)
new server / disaster (no extension needed on the target; needs pg_restore):
  pgbx backups    --from-s3 --db X S3FLAGS   list X's dumps in S3, newest first
  pgbx db-restore --from-s3 --db X --into NEWDB [--backup KEY | --time TS] S3FLAGS
      S3FLAGS: --s3-endpoint URL --s3-bucket B [--s3-region R] --server-name S --credentials-file F
      newest dump at or before TS (default: newest) -> CREATE DATABASE NEWDB (refused if it exists) -> pg_restore
policy / access (show with no arguments; changes that reduce protection need --yes):
  pgbx schedule [TEXT]            pgbx retention [--max-backups N] [--max-days N]
  pgbx pause --reason TEXT --yes  pgbx resume
  pgbx scope [--include P1,P2] [--exclude P1,P2] [--reset]
  pgbx verify-schedule TEXT|never pgbx link [--backup-id N] [--expires '1 hour']
  pgbx overview                   (admin database: every database on the server)
setup (guarded: shows the plan; --yes writes):
  pgbx setup server [--s3-endpoint U --s3-bucket B --s3-region R --server-name S --credentials-file F
              --access-key-env VAR --secret-key-env VAR --pg-conf FILE] [--yes]
      on the DB host, with sudo: writes <config dir>/conf.d/pgbx.conf (shared_preload_libraries merged with
      what is loaded) and the credentials file (0600, owner postgres; keys read from env vars, default
      AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY); prints the ONE restart command, never restarts Postgres.
      `pgbx setup` alone is the same as `pgbx setup server`.
  pgbx setup client [NAME] [--host H --port P | --ssh T [--ssh-port N --ssh-jump J]] [--user U] [--db D]
              [--s3-endpoint U --s3-bucket B --s3-region R --server-name S --credentials-file F] [--no-skill] [--yes]
      on your laptop, no sudo: asks (or takes flags), saves the profile (default if first), tests it
      (connect, extension version, status()), prints next steps, offers `pgbx skill install` on a terminal
profiles (one per server; never stores passwords or S3 keys):
  pgbx profile add NAME [--host H --port P --user U --admin-db D --s3-endpoint U --s3-bucket B --s3-region R
                         --server-name S --credentials-file F --ssh T --ssh-port N --ssh-jump J
                         --tunnel-idle 10m]   (the first profile becomes the default)
  pgbx profile list | show NAME | remove NAME | use NAME          (use = set the default)
  --profile NAME or PGBX_PROFILE on any command; precedence: flag > PGHOST/PGPORT/PGUSER > profile > default
agent memory (${PGBX_MEMORY_DIR:-~/pgbx}/<connection>/<db>/memories.md + tables.md; connection = profile):
  pgbx memories export [FILE | -] [--db D]    one JSON bundle (default pgbx-memories-<connection>.json)
  pgbx memories import FILE [--as CONNECTION] [--overwrite]   differing local files are kept unless --overwrite
  pgbx memories path                          where this connection's memory lives
agent skill:
  pgbx skill install [--no-codex] | uninstall | where
read queries (one statement, inside BEGIN READ ONLY, then ROLLBACK):
  pgbx query \"SQL\" [--db D] [--max-rows 1000] [--timeout 30s]
      SELECT/WITH/TABLE/VALUES/SHOW/EXPLAIN only; refuses writes, row locks and side-effect functions
      (a best-effort guard for agents, not a security boundary: give the user a read-only role yourself)
      -> columns[{name,type}], rows[], row_count, truncated
over ssh (system ssh; keys/agent/~/.ssh/config are ssh's business):
  --ssh user@host [--ssh-port N] [--ssh-jump J]   tunnels Postgres; doctor/logs/diagnose/setup run there
  pgbx tunnel [open] | list | close [NAME | --all]
      the forward is shared by later commands and closes after --tunnel-idle (default 10m) unused
common: --json --profile NAME --host --port --user --admin-db --timeout SECS (PGHOST/PGPORT/PGUSER/PGPASSWORD honoured)
TS always carries a UTC offset: '2026-01-31 14:00:00+00'";

// ---------------------------------------------------------------- safety

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Level {
    ReadOnly,
    Safe,
    Guarded,
    Destructive,
}

pub fn level(cmd: &str, a: &Args) -> Level {
    let shows = a.pos.is_empty() && !["max-backups", "max-days", "include", "exclude", "reset"].iter().any(|f| a.has(f));
    match cmd {
        "now" | "verify" | "db-restore" | "resume" | "link" | "skill" => Level::Safe,
        "setup" if a.pos.first().map(String::as_str) == Some("client") => Level::Safe,
        "profile" if matches!(a.pos.first().map(String::as_str), Some("add" | "remove" | "use")) => Level::Safe,
        "memories" if a.pos.first().map(String::as_str) == Some("import") => Level::Safe,
        "schedule" if shows => Level::ReadOnly,
        "schedule" => Level::Safe,
        "retention" | "scope" if shows => Level::ReadOnly,
        "retention" | "scope" | "pause" | "setup" => Level::Guarded,
        "verify-schedule" if a.pos.first().and_then(|s| policy::verify_schedule_risk(s)).is_some() => Level::Guarded,
        "verify-schedule" => Level::Safe,
        _ => Level::ReadOnly,
    }
}

/// A restore target time must carry an explicit UTC offset, or it silently means the server's local zone.
pub fn check_time(t: &str) -> Result<(), String> {
    let t = t.trim();
    let bad = || Err(format!("--time '{t}' has no UTC offset; write it like '2026-01-31 14:00:00+00' or '2026-01-31T14:00:00Z'"));
    let Some(c) = t.find(':') else { return bad() };
    if t.ends_with('Z') || t.ends_with('z') {
        return Ok(());
    }
    let tail = &t[c..];
    let Some(i) = tail.rfind(['+', '-']) else { return bad() };
    let off = tail[i + 1..].replace(':', "");
    if (off.len() == 2 || off.len() == 4) && off.chars().all(|c| c.is_ascii_digit()) {
        Ok(())
    } else {
        bad()
    }
}

/// db-restore must go into a database that does not exist yet and is not the source.
pub fn check_new_db(src: &str, into: &str, exists: bool) -> Result<(), String> {
    if into.is_empty() {
        return Err("--into NEWDB is required".into());
    }
    if into == src {
        return Err(format!("refusing: --into must be a NEW database, not the source '{src}'"));
    }
    if exists {
        return Err(format!("refusing: database '{into}' already exists; pick a new name (the live database is never overwritten)"));
    }
    Ok(())
}

// ---------------------------------------------------------------- context

struct Ctx {
    a: Args,
    admin_db: Option<String>,
    tunnel: std::sync::OnceLock<u16>,
}

impl Ctx {
    fn db(&self) -> String {
        self.a.get("db").unwrap_or("postgres").to_string()
    }

    /// host, port, user to connect to: flags/env/defaults, or the local end of the ssh tunnel (opened once).
    fn target(&self) -> Result<(String, u16, String), String> {
        let env = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());
        let user = self.a.get("user").map(String::from).or(env("PGUSER")).unwrap_or("postgres".into());
        if self.a.has("ssh") {
            if self.tunnel.get().is_none() {
                let _ = self.tunnel.set(tunnel::ensure(&self.a, self.a.get("profile"))?);
            }
            return Ok(("127.0.0.1".into(), *self.tunnel.get().unwrap(), user));
        }
        let host = self.a.get("host").map(String::from).or(env("PGHOST")).unwrap_or(DEFAULT_HOST.into());
        let port: u16 = self.a.get("port").map(String::from).or(env("PGPORT")).unwrap_or("5432".into())
            .parse().map_err(|_| "bad --port".to_string())?;
        Ok((host, port, user))
    }

    fn connect(&self, db: &str) -> Result<Client, String> {
        let env = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());
        let (host, port, user) = self.target()?;
        let mut c = postgres::Config::new();
        c.host(&host).port(port).user(&user).dbname(db).application_name("pgbx").connect_timeout(Duration::from_secs(5));
        if let Some(pw) = env("PGPASSWORD") {
            c.password(pw);
        }
        c.connect(NoTls).map_err(|e| format!("cannot connect to Postgres ({host}:{port}, db {db}): {e}"))
    }

    fn admin_db(&mut self) -> String {
        if let Some(a) = &self.admin_db {
            return a.clone();
        }
        let a = self.a.get("admin-db").map(String::from).or_else(|| {
            let mut c = self.connect("postgres").ok()?;
            let v: Option<String> = c.query_one("SELECT nullif(current_setting('pgbx.admin_db', true), '')", &[]).ok()?.get(0);
            v
        }).unwrap_or("postgres".into());
        self.admin_db = Some(a.clone());
        a
    }

    fn pgdata(&self) -> Option<String> {
        self.a.get("pgdata").map(String::from)
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(self.a.get("timeout").and_then(|s| s.parse().ok()).unwrap_or(3600))
    }
}

/// Run a query and get its rows as JSON objects.
fn rows(c: &mut Client, sql: &str, p: &[&(dyn ToSql + Sync)]) -> Result<Vec<Value>, String> {
    let q = format!("SELECT coalesce(json_agg(t), '[]')::text FROM ({sql}) t");
    let s: String = c.query_one(&q, p).map_err(pe)?.get(0);
    match serde_json::from_str(&s).map_err(|e| e.to_string())? {
        Value::Array(v) => Ok(v),
        _ => Ok(vec![]),
    }
}

fn one(c: &mut Client, sql: &str, p: &[&(dyn ToSql + Sync)]) -> Result<Value, String> {
    Ok(rows(c, sql, p)?.into_iter().next().unwrap_or(Value::Null))
}

fn pe(e: postgres::Error) -> String {
    match e.as_db_error() {
        Some(d) => d.message().to_string(),
        None => e.to_string(),
    }
}

fn wait_job(c: &mut Client, id: i64, timeout: Duration) -> Result<Value, String> {
    let t0 = Instant::now();
    loop {
        let r = one(c, "SELECT id, kind, state, started, finished, s3_key, bytes, error, params FROM pgbx.history WHERE id = $1", &[&id])?;
        match r["state"].as_str() {
            Some("done") | Some("failed") | Some("expired") => return Ok(r),
            None => return Err(format!("job {id} not found in pgbx.history")),
            _ => {}
        }
        if t0.elapsed() > timeout {
            return Err(format!("timed out waiting for job {id} (state {})", r["state"]));
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

// ---------------------------------------------------------------- commands

type Out = Result<Value, String>;

fn cmd_status(cx: &mut Ctx) -> Out {
    let db = cx.db();
    let mut c = cx.connect(&db).map_err(|e| format!("{e} — if Postgres is down, run pgbx diagnose"))?;
    let st = one(&mut c, "SELECT * FROM pgbx.status()", &[])?;
    Ok(json!({"ok": true, "postgres": "up", "database": db, "status": st}))
}

fn cmd_list(cx: &mut Ctx) -> Out {
    let db = cx.db();
    let mut c = cx.connect(&db)?;
    let v = rows(&mut c, "SELECT id, taken_at, age::text, trigger, size, s3_key FROM pgbx.backups", &[])?;
    Ok(json!({"ok": true, "database": db, "database_backups": v}))
}

fn cmd_backups(cx: &mut Ctx) -> Out {
    if !cx.a.has("from-s3") {
        return Err("pgbx backups lists dumps straight from S3: add --from-s3 and the S3 flags (or use pgbx list --db X)".into());
    }
    s3restore::backups(cx)
}

fn queue(cx: &mut Ctx, db: &str, sql: &str, p: &[&(dyn ToSql + Sync)]) -> Out {
    let mut c = cx.connect(db)?;
    let id: i64 = c.query_one(sql, p).map_err(pe)?.get(0);
    let mut out = json!({"ok": true, "database": db, "job_id": id, "state": "queued",
        "watch": format!("SELECT * FROM pgbx.history WHERE id = {id}")});
    if cx.a.has("wait") {
        let r = wait_job(&mut c, id, cx.timeout())?;
        out["ok"] = json!(r["state"] == "done");
        out["state"] = r["state"].clone();
        out["job"] = r;
    }
    Ok(out)
}

fn cmd_now(cx: &mut Ctx) -> Out {
    let db = cx.db();
    queue(cx, &db, "SELECT pgbx.backup_now()", &[])
}

fn cmd_verify(cx: &mut Ctx) -> Out {
    let db = cx.db();
    queue(cx, &db, "SELECT pgbx.verify_now()", &[])
}

fn cmd_db_restore(cx: &mut Ctx) -> Out {
    if cx.a.has("from-s3") {
        return s3restore::db_restore(cx);
    }
    if cx.a.has("backup") {
        return Err("--backup KEY works with --from-s3 only".into());
    }
    let db = cx.a.get("db").ok_or("--db SOURCE is required")?.to_string();
    let into = cx.a.get("into").unwrap_or("").to_string();
    let mut c = cx.connect(&db)?;
    let exists: bool = c.query_one("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)", &[&into]).map_err(pe)?.get(0);
    check_new_db(&db, &into, exists)?;
    let time = cx.a.get("time").map(String::from);
    if let Some(t) = &time {
        check_time(t)?;
    }
    drop(c);
    queue(cx, &db, "SELECT pgbx.restore($1, coalesce(($2::text)::timestamptz, now()))", &[&into, &time])
}

fn check(name: &str, ok: bool, detail: impl Into<String>, fix: &str) -> Value {
    json!({"name": name, "ok": ok, "detail": detail.into(), "fix": if ok { "" } else { fix }})
}

fn cmd_doctor(cx: &mut Ctx) -> Out {
    let mut ch = vec![];
    let conn = cx.connect("postgres");
    let pg_up = conn.is_ok();
    ch.push(match &conn {
        Ok(_) => check("postgres reachable", true, "connected", ""),
        Err(e) => check("postgres reachable", false, e.clone(), "start Postgres, or check --host/--port/--user; `pgbx diagnose` says why it is down"),
    });
    let mut data_dir = cx.pgdata();
    if let Ok(mut c) = conn {
        let admin = cx.admin_db();
        // the server runs its own checks in SQL; pgbx only adds local ones on top
        match cx.connect(&admin).and_then(|mut a| rows(&mut a, "SELECT * FROM pgbx.doctor()", &[])) {
            Ok(v) => {
                for r in v {
                    ch.push(check(r["name"].as_str().unwrap_or("?"), r["ok"] == true, scalar(&r["detail"]), r["fix"].as_str().unwrap_or("")));
                }
            }
            Err(e) => ch.push(check("server checks", false, e,
                "make sure pgbx is in shared_preload_libraries (the worker creates the extension in the admin database)")),
        }
        if let Some(d) = c.query_one("SHOW data_directory", &[]).ok().and_then(|r| r.get::<_, Option<String>>(0)) {
            data_dir = Some(d);
        }
    }
    let sk = skill::system_paths(false).map(|p| skill::installed_version(&p));
    let skv = sk.ok().flatten();
    let mut w = check("agent skill installed and same version", skv.as_deref() == Some(skill::VERSION),
        format!("installed {} / pgbx {}", skv.as_deref().unwrap_or("none"), skill::VERSION), "pgbx skill install");
    w["warning"] = json!(true);
    ch.push(w);
    if let Some(d) = &data_dir {
        if let Some((pct, free)) = diagnose::df(std::path::Path::new(d)) {
            ch.push(check("disk free (data dir)", pct < 90, format!("{} free, {pct}% used", diagnose::human(free)),
                "free disk space or grow the volume; a full disk stops Postgres"));
        }
    }
    let healthy = ch.iter().all(|c| c["ok"] == true || c["warning"] == true);
    let mut v = json!({"ok": healthy, "healthy": healthy, "postgres_up": pg_up, "checks": ch});
    if !pg_up {
        let d = run_diagnose(cx, false);
        v["checks"].as_array_mut().unwrap().insert(1, check("why postgres is down", false,
            format!("{} ({}); {} evidence line(s)", scalar(&d["probable_cause"]), scalar(&d["postgres"]),
                d["evidence"].as_array().map(|a| a.len()).unwrap_or(0)),
            "see `diagnosis.steps` (pgbx diagnose): each step has a safety tier; destructive ones need human approval"));
        v["diagnosis"] = d;
    }
    Ok(v)
}

fn run_diagnose(cx: &mut Ctx, probe: bool) -> Value {
    let up = probe && cx.connect("postgres").is_ok();
    let inp = diagnose::Input { pgdata: cx.pgdata(), log_file: cx.a.get("log").map(String::from), postgres_up: up };
    diagnose::diagnose(&inp)
}

fn cmd_diagnose(cx: &mut Ctx) -> Out {
    Ok(run_diagnose(cx, true))
}

fn cmd_logs(cx: &mut Ctx) -> Out {
    let n: i64 = cx.a.get("lines").and_then(|s| s.parse().ok()).unwrap_or(10);
    let admin = cx.admin_db();
    let mut c = cx.connect(&admin)?;
    let failed = rows(&mut c,
        "SELECT id, kind, trigger, requested_at, finished, error FROM pgbx.history WHERE state = 'failed' ORDER BY id DESC LIMIT $1", &[&n])?;
    Ok(json!({"ok": true, "database": admin, "recent_failures": failed,
        "note": "per-database failures live in each database: pgbx logs --admin-db <db>; the worker also logs to the Postgres log (prefix 'pgbx:')"}))
}

// ---------------------------------------------------------------- output

fn human(v: &Value, ind: usize, out: &mut String) {
    let pad = "  ".repeat(ind);
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                match x {
                    Value::Object(_) | Value::Array(_) => {
                        out.push_str(&format!("{pad}{k}:\n"));
                        human(x, ind + 1, out);
                    }
                    _ => out.push_str(&format!("{pad}{k}: {}\n", scalar(x))),
                }
            }
        }
        Value::Array(a) => {
            if a.is_empty() {
                out.push_str(&format!("{pad}(none)\n"));
            }
            for x in a {
                match x {
                    Value::Object(_) | Value::Array(_) => {
                        out.push_str(&format!("{pad}-\n"));
                        human(x, ind + 1, out);
                    }
                    _ => out.push_str(&format!("{pad}- {}\n", scalar(x))),
                }
            }
        }
        _ => out.push_str(&format!("{pad}{}\n", scalar(v))),
    }
}

fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "-".into(),
        x => x.to_string(),
    }
}

fn doctor_text(v: &Value) -> String {
    let mut s = String::new();
    for c in v["checks"].as_array().cloned().unwrap_or_default() {
        let ok = c["ok"] == true;
        s.push_str(&format!("[{}] {}: {}\n", if ok { " ok " } else if c["warning"] == true { "warn" } else { "FAIL" }, scalar(&c["name"]), scalar(&c["detail"])));
        if !ok {
            s.push_str(&format!("       fix: {}\n", scalar(&c["fix"])));
        }
    }
    s.push_str(if v["healthy"] == true { "healthy\n" } else { "NOT healthy\n" });
    s
}

/// m2: usage errors are one JSON object too when --json was asked for.
pub fn usage_error_json(raw: &[String], e: &str) -> Option<String> {
    raw.iter().any(|x| x == "--json").then(|| json!({"ok": false, "error": e, "usage": "pgbx help"}).to_string())
}

fn usage_error(raw: &[String], e: &str) {
    match usage_error_json(raw, e) {
        Some(j) => println!("{j}"),
        None => eprintln!("pgbx: {e}"),
    }
}

fn cmd_skill(cx: &mut Ctx) -> Out {
    let p = skill::system_paths(cx.a.has("no-codex"))?;
    match cx.a.pos.first().map(String::as_str) {
        Some("install") => skill::install(&p),
        Some("uninstall") => skill::uninstall(&p),
        Some("where") | None => Ok(skill::where_(&p)),
        Some(x) => Err(format!("unknown skill action '{x}' (install | uninstall | where)")),
    }
}

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let a = match parse_args(raw.clone()) {
        Ok(a) => a,
        Err(e) => {
            usage_error(&raw, &e);
            std::process::exit(2);
        }
    };
    if a.has("version") {
        if a.has("json") {
            println!("{}", json!({"ok": true, "version": skill::VERSION}));
        } else {
            println!("pgbx {}", skill::VERSION);
        }
        return;
    }
    let as_json = a.has("json");
    if a.cmd == "help" || a.has("help") {
        println!("{HELP}");
        return;
    }
    let mut a = a;
    let cmd = a.cmd.clone();
    let lvl = level(&cmd, &a);
    if cmd == "tunnel" && a.has("serve") {
        std::process::exit(tunnel::serve(&a)); // the detached helper: its flags are complete, no profile lookup
    }
    let setup_sub = if cmd == "setup" { a.pos.first().cloned() } else { None };
    let prof = if cmd == "profile" || setup_sub.as_deref() == Some("client") { Ok(None) } else { profile::apply_from_disk(&mut a) };
    let mut cx = Ctx { a, admin_db: None, tunnel: Default::default() };
    let r = match cmd.as_str() {
        _ if prof.is_err() => Err(prof.clone().unwrap_err()),
        "profile" => profile::run_sys(&cx.a),
        "memories" => memories::run_sys(&cx.a, prof.clone().ok().flatten()),
        c if tunnel::runs_remotely(c, &cx.a) => tunnel::run_remote(c, &cx.a),
        "query" => query::run(&mut cx),
        "tunnel" => tunnel::run(&cx.a, cx.a.get("profile")),
        "status" => cmd_status(&mut cx),
        "list" => cmd_list(&mut cx),
        "now" => cmd_now(&mut cx),
        "verify" => cmd_verify(&mut cx),
        "backups" => cmd_backups(&mut cx),
        "db-restore" => cmd_db_restore(&mut cx),
        "doctor" => cmd_doctor(&mut cx),
        "logs" => cmd_logs(&mut cx),
        "schedule" => policy::schedule(&mut cx),
        "retention" => policy::retention(&mut cx),
        "pause" => policy::pause(&mut cx),
        "resume" => policy::resume(&mut cx),
        "scope" => policy::scope(&mut cx),
        "verify-schedule" => policy::verify_schedule(&mut cx),
        "link" => policy::link(&mut cx),
        "overview" => policy::overview(&mut cx),
        "skill" => cmd_skill(&mut cx),
        "diagnose" => cmd_diagnose(&mut cx),
        "ui" => ui::run(&mut cx),
        "setup" => match setup_sub.as_deref() {
            Some("client") => setup_client::run(&mut cx),
            Some("server") | None => setup::run(&mut cx),
            Some(x) => Err(format!("unknown setup '{x}' (pgbx setup server | pgbx setup client)")),
        },
        _ => unreachable!(),
    };
    let mut v = r.unwrap_or_else(|e| json!({"ok": false, "error": e}));
    v["command"] = json!(cmd);
    v["safety"] = json!(format!("{lvl:?}").to_lowercase());
    if cmd == "setup" && setup_sub.is_none() {
        v["hint"] = json!("same as pgbx setup server");
        if !as_json {
            eprintln!("pgbx setup: same as pgbx setup server");
        }
    }
    if let Ok(Some(p)) = &prof {
        v["profile_used"] = json!(p);
    }
    let ok = v["ok"] == true;
    if as_json {
        println!("{v}");
    } else if cmd == "link" && ok {
        println!("{}", scalar(&v["url"])); // bare URL so URL=$(pgbx link) works
    } else if cmd == "doctor" && v.get("checks").is_some() {
        print!("{}", doctor_text(&v));
        if v.get("diagnosis").is_some() {
            print!("\n{}", diagnose::text(&v["diagnosis"]));
        }
    } else if cmd == "diagnose" && ok {
        print!("{}", diagnose::text(&v));
    } else if let Some(e) = v.get("error").filter(|_| !ok) {
        eprintln!("pgbx {cmd}: {}", scalar(e));
        if v.get("steps").is_some() {
            let mut s = String::new();
            human(&v["steps"], 1, &mut s);
            eprint!("steps:\n{s}");
        }
    } else {
        let mut s = String::new();
        human(&v, 0, &mut s);
        print!("{s}");
    }
    std::process::exit(if ok { 0 } else { 1 });
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &[&str]) -> Args {
        parse_args(s.iter().map(|x| x.to_string())).unwrap()
    }
    #[test]
    fn parses_commands_and_flags() {
        let a = p(&["now", "--db", "shop", "--wait", "--json"]);
        assert_eq!(a.cmd, "now");
        assert_eq!(a.get("db"), Some("shop"));
        assert!(a.has("wait") && a.has("json"));
        let a = p(&["db-restore", "--from-s3", "--time=2026-01-01 10:00:00+00", "--backup", "k"]);
        assert_eq!(a.get("time"), Some("2026-01-01 10:00:00+00"));
        assert!(a.has("from-s3") && a.get("backup") == Some("k"));
        assert_eq!(p(&[]).cmd, "help");
    }

    #[test]
    fn setup_is_guarded_and_parses() {
        let a = p(&["setup", "--s3-endpoint", "https://s3.x", "--s3-bucket", "b", "--access-key-env", "K", "--secret-key-env", "S",
                    "--pg-conf", "/etc/postgresql/16/main/postgresql.conf", "--yes", "--json"]);
        assert_eq!(a.cmd, "setup");
        assert_eq!(a.get("access-key-env"), Some("K"));
        assert_eq!(level("setup", &a), Level::Guarded);
        assert!(usage_error_json(&["setup".into(), "--json".into()], "x").unwrap().contains("\"ok\":false"));
    }

    #[test]
    fn rejects_bad_args() {
        let e = |s: &[&str]| parse_args(s.iter().map(|x| x.to_string())).unwrap_err();
        assert!(e(&["frobnicate"]).contains("unknown command"));
        assert!(e(&["status", "--nope"]).contains("unknown option"));
        assert!(e(&["now", "--db"]).contains("needs a value"));
        assert!(e(&["now", "--wait=yes"]).contains("takes no value"));
    }

    #[test]
    fn safety_levels() {
        for c in ["status", "list", "doctor", "logs", "backups"] {
            assert_eq!(level(c, &p(&[c])), Level::ReadOnly);
        }
        for c in ["now", "verify", "db-restore"] {
            assert_eq!(level(c, &p(&[c])), Level::Safe);
        }
        assert_eq!(level("db-restore", &p(&["db-restore", "--from-s3"])), Level::Safe);
    }

    #[test]
    fn db_restore_only_into_new_db() {
        assert!(check_new_db("shop", "shop_r", false).is_ok());
        assert!(check_new_db("shop", "shop", false).unwrap_err().contains("NEW"));
        assert!(check_new_db("shop", "other", true).unwrap_err().contains("already exists"));
        assert!(check_new_db("shop", "", false).is_err());
    }

    #[test]
    fn usage_errors_are_json_with_flag() {
        let raw: Vec<String> = ["now", "--bogus", "--json"].iter().map(|s| s.to_string()).collect();
        let e = parse_args(raw.clone()).unwrap_err();
        let v: Value = serde_json::from_str(&usage_error_json(&raw, &e).unwrap()).unwrap();
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("--bogus"));
        assert_eq!(usage_error_json(&raw[..2], &e), None);
    }

    #[test]
    fn time_needs_utc_offset() {
        for ok in ["2026-01-31 14:00:00+00", "2026-01-31T14:00:00Z", "2026-01-31 14:00-05:30", "2026-01-31 14:00:00+0200"] {
            assert!(check_time(ok).is_ok(), "{ok}");
        }
        for bad in ["2026-01-31 14:00:00", "2026-01-31", "yesterday", "2026-01-31 14:00:00+5"] {
            assert!(check_time(bad).unwrap_err().contains("UTC offset"), "{bad}");
        }
    }

    #[test]
    fn policy_levels() {
        assert_eq!(level("retention", &p(&["retention"])), Level::ReadOnly);
        assert_eq!(level("retention", &p(&["retention", "--max-days", "3"])), Level::Guarded);
        assert_eq!(level("pause", &p(&["pause"])), Level::Guarded);
        assert_eq!(level("resume", &p(&["resume"])), Level::Safe);
        assert_eq!(level("verify-schedule", &p(&["verify-schedule", "never"])), Level::Guarded);
        assert_eq!(level("schedule", &p(&["schedule"])), Level::ReadOnly);
        assert_eq!(level("profile", &p(&["profile", "list"])), Level::ReadOnly);
        assert_eq!(level("profile", &p(&["profile", "add", "x", "--host", "h"])), Level::Safe);
    }

    #[test]
    fn profile_flag_parses() {
        let a = p(&["status", "--profile", "prod", "--json"]);
        assert_eq!(a.get("profile"), Some("prod"));
    }

}

pub(crate) fn sh_out(cmd: &str, args: &[&str]) -> Option<String> {
    let o = std::process::Command::new(cmd).args(args).output().ok().filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn is_root() -> bool {
    sh_out("id", &["-u"]).as_deref() == Some("0")
}
