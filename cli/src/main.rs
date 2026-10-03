//! pgbx — the pgbx command line. Uses SQL while Postgres is up; diagnoses it when it is down; restores
//! databases straight from S3 onto a new server (no extension needed there). Every command takes --json and
//! prints one JSON object.
//!
//! Safety levels (enforced here, see `Level`):
//!   read-only   status, list, backups, doctor, diagnose, logs, overview, "show" forms of policy commands
//!   safe        now, verify, db-restore (NEW db only, also --from-s3), resume, link, schedule, skill
//!   guarded     pause, lowering retention, narrowing scope, verify-schedule never — require --yes

mod adapter;
mod client_only;
mod config;
mod conn;
#[path = "../../src/crypt.rs"]
pub mod crypt;
mod decrypt;
mod diagnose;
#[path = "../../src/globals.rs"]
pub mod globals;
mod jobs;
mod load;
mod memories;
mod metrics;
mod pitr;
mod pitrcmd;
mod policy;
mod profile;
mod query;
#[path = "../../src/s3auth.rs"]
pub mod s3auth;
mod s3restore;
mod s3x;
mod serve;
mod setup;
mod setup_client;
mod skill;
mod tls;
mod ui;
mod vars;
mod wal;

use postgres::types::ToSql;
use postgres::Client;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Unix: the Debian/RHEL socket dir. Windows has no unix socket default, so TCP to localhost.
#[cfg(unix)]
pub(crate) const DEFAULT_HOST: &str = "/var/run/postgresql";
#[cfg(not(unix))]
pub(crate) const DEFAULT_HOST: &str = "localhost";

// ---------------------------------------------------------------- arguments

const BOOL_FLAGS: &[&str] = &[
    "json", "wait", "from-s3", "help", "yes", "reset", "no-codex", "version", "strict", "all", "no-skill", "overwrite", "apply",
    "with-roles", "pitr", "expire", "async-daemon", "prefetch-daemon", "yes-replace-whole-server", "no-open", "allow-safe",
];
const VALUE_FLAGS: &[&str] = &[
    "db", "into", "time", "backup", "pgdata", "host", "port", "user", "admin-db", "timeout", "lines", "reason", "max-backups",
    "max-days", "include", "exclude", "backup-id", "expires", "log", "s3-endpoint", "s3-bucket", "s3-region", "server-name",
    "credentials-file", "credentials", "listen", "access-key-env", "secret-key-env", "pg-conf", "profile", "url", "adapter", "adapter-command",
    "max-rows", "as", "hours", "gate", "key-file", "roles", "gfs", "in", "out", "conf", "target",
    "system-id",
];
/// Removed in 0.6 (ADR 0003): pgbx has no built-in SSH any more.
const REMOVED_SSH: &str = "pgbx has no built-in SSH any more (0.6, ADR 0003): use the ssh adapter, e.g. \
    pgbx profile add prod --adapter ssh target=user@host  (see pgbx help, and the guide \"Connect through SSH, AWS, GCP, Azure or your own adapter\")";
const COMMANDS: &[&str] = &[
    "status", "list", "backups", "now", "verify", "db-restore", "doctor", "logs", "help", "schedule", "retention",
    "pause", "resume", "scope", "verify-schedule", "link", "overview", "skill", "diagnose", "ui", "setup", "profile", "query", "memories",
    "jobs", "load", "serve", "metrics", "decrypt", "pitr", "wal-push", "wal-get",
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
            } else if ["ssh", "ssh-port", "ssh-jump", "tunnel-idle"].contains(&name.as_str()) {
                return Err(format!("--{name}: {REMOVED_SSH}"));
            } else {
                return Err(format!("unknown option --{name} (see pgbx help)"));
            }
        } else if s == "-h" {
            a.flags.insert("help".into(), String::new());
        } else if a.cmd.is_empty() {
            if s == "tunnel" {
                return Err(format!("pgbx tunnel: {REMOVED_SSH}"));
            }
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
  pgbx jobs                                  the server-wide job queue: what runs (slot / restore lane), what waits and why
  pgbx load     [--db X]                     the load gate: last load sample, thresholds, per database what it would
                                             defer (shadow, the default), deferred (on) or forced
  pgbx ui       [--listen 127.0.0.1:8432] [--strict]
                                             read-only audit web UI (overview, 30-day timeline, health) and
                                             Prometheus GET /metrics; --strict refuses a role that could change backups
  pgbx serve    [--profile P] [--listen 127.0.0.1:0] [--no-open] [--allow-safe]
                                             local web app (overview, database detail, restore helper, read-only
                                             query, health); random port + per-run token, opens the browser;
                                             --allow-safe enables backup now / verify now / restore into a NEW db /
                                             cancel a queued job, each confirmed (never guarded or destructive)
  pgbx metrics                               the same Prometheus metrics once, as text
  pgbx decrypt  --key-file F [--in FILE] [--out FILE]
                                             decrypt an encrypted dump (e.g. from a download link; stdin -> stdout)
safe:
  pgbx now      [--db X] [--wait]            queue a backup
  pgbx verify   [--db X] [--wait]            queue a restore test
  pgbx db-restore --db X --into NEWDB [--time TS] [--with-roles [--roles referenced|all]] [--wait]
                                             restore into a NEW database on this server (via the extension);
                                             --with-roles first creates missing roles from the backup's roles file
new server / disaster (no extension needed on the target; needs pg_restore):
  pgbx backups    --from-s3 --db X S3FLAGS   list X's dumps in S3, newest first
  pgbx db-restore --from-s3 --db X --into NEWDB [--backup KEY | --time TS] [--with-roles [--roles R]]
                  [--key-file F] S3FLAGS      (--key-file: the pgbx.encryption_key_file of encrypted backups)
      S3FLAGS: --s3-endpoint URL --s3-bucket B [--s3-region R] --server-name S [--credentials-file F]
               (F: access_key_id=/secret_access_key= lines; absent or aws-default: AWS_ACCESS_KEY_ID & co., web
               identity, container credentials, then the EC2 instance role via IMDSv2)
      newest dump at or before TS (default: newest) -> CREATE DATABASE NEWDB (refused if it exists) -> pg_restore
policy / access (show with no arguments; changes that reduce protection need --yes):
  pgbx schedule [TEXT]            pgbx retention [--max-backups N] [--max-days N] [--gfs 7d,4w,12m|off]
  pgbx schedule suggest [--db X] [--hours N] [--apply [--yes]]
                                  the quietest window learned from activity + the configure() call to copy;
                                  never applied by itself (--apply asks y/N on a terminal, else needs --yes)
  pgbx pause --reason TEXT --yes  pgbx resume
  pgbx scope [--include P1,P2] [--exclude P1,P2] [--reset]
  pgbx verify-schedule TEXT|never pgbx link [--backup-id N] [--expires '1 hour']
  pgbx overview                   (admin database: every database on the server)
  pgbx jobs cancel ID [--db X] --yes        cancel a queued or running job (a running one is stopped, nothing left in S3)
  pgbx load --gate off|shadow|on|default --db X   the load gate for one database (on needs --yes)
point-in-time restore (optional, whole server; per-database dumps stay the default):
  pgbx setup pitr [--yes]                    archive_mode=on, archive_command='<this pgbx> wal-push %p', pgbx.pitr=on
                                             (ALTER SYSTEM; refuses a foreign archive_command; restart Postgres once)
  pgbx pitr status                           window (restorable from .. until), last base backup, gaps, backlog
  pgbx pitr backup-now [--wait]              queue a base backup (superuser)
  pgbx pitr list   (--conf FILE | S3FLAGS [--system-id N])   base backups + gaps straight from S3
  pgbx pitr restore --time TS|latest --target DIR (--conf FILE | S3FLAGS [--system-id N])
                                             newest base backup before TS -> DIR + recovery settings; never starts
                                             Postgres (prints the command); refuses a time inside a WAL gap; a copy
                                             gets archive_mode=off. --yes-replace-whole-server: DIR is a STOPPED
                                             server's data directory, moved aside (not deleted) first
  pgbx wal-push %p [--conf FILE]             archive_command    pgbx wal-get %f %p --conf FILE   restore_command
setup (guarded: shows the plan; --yes writes; `pgbx setup pitr` is above):
  pgbx setup server [--s3-endpoint U --s3-bucket B --s3-region R --server-name S --credentials-file F
              --access-key-env VAR --secret-key-env VAR --pg-conf FILE] [--credentials aws-default] [--yes]
      on the DB host, with sudo: writes <config dir>/conf.d/pgbx.conf (shared_preload_libraries merged with
      what is loaded) and the credentials file (0600, owner postgres; keys read from env vars, default
      AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY); prints the ONE restart command, never restarts Postgres.
      --credentials aws-default: no keys file; pgbx.credentials_file = 'aws-default' (EC2 instance role via IMDSv2).
      `pgbx setup` alone is the same as `pgbx setup server`.
  pgbx setup client [NAME] [--url URL | --adapter A [key=value ...] | --host H --port P --user U] [--db D]
              [--s3-endpoint U --s3-bucket B --s3-region R --server-name S --credentials-file F] [--no-skill] [--yes]
      on your laptop, no sudo: asks (or takes flags), saves the profile (default if first), tests it
      (connect, extension version, status()), prints next steps, offers `pgbx skill install` on a terminal
connections and profiles (<config dir>/config.yaml, 0600; profiles hold $VAR references, never secrets):
  a profile is a connection string or an adapter (any command that hands pgbx a connection string:
  ssh, aws, gcp, azure examples ship in the repo's adapters/, or your own)
  pgbx profile add NAME --url 'postgres://user:$PGPASSWORD@host:5432/db'
  pgbx profile add NAME --adapter A [--adapter-command CMD] [key=value ...]   (the adapter's own settings)
               [--admin-db D --s3-endpoint U --s3-bucket B --s3-region R --server-name S --credentials-file F]
  pgbx profile edit NAME [key=value | key= | --url U | --adapter A | --s3-... ]   (key= removes a setting)
  pgbx profile list | show NAME | remove NAME | use NAME   (the first profile, or `use`, is the default)
  $VAR / ${VAR} anywhere in a profile or url is expanded at run time ($$ = a literal $) from the environment,
  then the `secrets:` source in config.yaml (env | a .env file | a handler command run as `CMD NAME`)
  which connection: --url > --profile > --host/--port > PGBX_URL > PGBX_PROFILE > default profile > PGHOST/...
  an adapter starts with the command and stops when it ends (pgbx serve keeps it for its whole run);
  doctor checks over SQL through it; diagnose and setup server run on the database host itself
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
common: --json --profile NAME --url URL --host --port --user --admin-db --timeout SECS
        (PGBX_URL, PGBX_PROFILE, PGHOST/PGPORT/PGUSER honoured; PGPASSWORD when the url has no password)
        TLS: libpq sslmode (default prefer; require, verify-ca, verify-full + sslrootcert) from the url,
        the profile's sslmode/sslrootcert keys, or PGSSLMODE/PGSSLROOTCERT
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
    let shows = a.pos.is_empty() && !["max-backups", "max-days", "gfs", "include", "exclude", "reset"].iter().any(|f| a.has(f));
    match cmd {
        "now" | "verify" | "db-restore" | "resume" | "link" | "skill" | "wal-push" | "wal-get" => Level::Safe,
        "setup" if a.pos.first().map(String::as_str) == Some("client") => Level::Safe,
        "pitr" => match a.pos.first().map(String::as_str) {
            Some("restore") if a.has("yes-replace-whole-server") => Level::Destructive,
            Some("restore" | "backup-now" | "backup" | "publish-gaps") => Level::Safe,
            Some("expire") => Level::Guarded,
            _ => Level::ReadOnly,
        },
        "profile" if matches!(a.pos.first().map(String::as_str), Some("add" | "edit" | "remove" | "use")) => Level::Safe,
        "memories" if a.pos.first().map(String::as_str) == Some("import") => Level::Safe,
        "schedule" if shows => Level::ReadOnly,
        "schedule" if a.pos.first().map(String::as_str) == Some("suggest") && !a.has("apply") => Level::ReadOnly,
        "schedule" => Level::Safe,
        "retention" | "scope" if shows => Level::ReadOnly,
        "retention" | "scope" | "pause" | "setup" => Level::Guarded,
        "verify-schedule" if a.pos.first().and_then(|s| policy::verify_schedule_risk(s)).is_some() => Level::Guarded,
        "verify-schedule" => Level::Safe,
        "jobs" if a.pos.first().map(String::as_str) == Some("cancel") => Level::Guarded,
        "load" => load::level_of(a),
        "serve" if a.has("allow-safe") => Level::Safe,
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

/// --roles: which roles of the backup's roles file --with-roles creates.
pub fn check_roles_scope(r: Option<&str>) -> Result<String, String> {
    match r.unwrap_or("referenced") {
        s @ ("referenced" | "all") => Ok(s.to_string()),
        s => Err(format!("--roles '{s}': use referenced (roles this database uses; default) or all")),
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
    /// how this command reaches Postgres (a url, an adapter, or flags/env); shared, so an adapter starts once
    conn: Arc<conn::Conn>,
}

impl Ctx {
    fn new(a: Args, conn: Arc<conn::Conn>) -> Ctx {
        Ctx { a, admin_db: None, conn }
    }

    /// --db, else the database named in the connection string, else postgres.
    fn db(&self) -> String {
        self.a.get("db").map(String::from).or_else(|| self.conn.default_db()).unwrap_or("postgres".into())
    }

    fn connect(&self, db: &str) -> Result<Client, String> {
        self.conn.connect(db)
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
            Some("done") | Some("failed") | Some("expired") | Some("cancelled") => return Ok(r),
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
    if let Some(v) = client_only::status_without(&client_only::ext(&mut c)?, &db) {
        return Ok(v);
    }
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
    if cx.a.has("key-file") {
        return Err("--key-file is for --from-s3 restores; on this server the worker uses pgbx.encryption_key_file".into());
    }
    if !cx.a.has("with-roles") {
        if cx.a.has("roles") {
            return Err("--roles works with --with-roles".into());
        }
        return queue(cx, &db, "SELECT pgbx.restore($1, coalesce(($2::text)::timestamptz, now()))", &[&into, &time]);
    }
    let roles = check_roles_scope(cx.a.get("roles"))?;
    queue(cx, &db, "SELECT pgbx.restore($1, coalesce(($2::text)::timestamptz, now()), true, $3)", &[&into, &time, &roles])
}

/// doctor() rows that give advice (a better schedule, estimates still settling) rather than report a fault
const ADVISORY_CHECKS: &[&str] = &["schedule_in_quiet_window", "eta_accuracy"];

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
                    let name = r["name"].as_str().unwrap_or("?");
                    let mut c = check(name, r["ok"] == true, scalar(&r["detail"]), r["fix"].as_str().unwrap_or(""));
                    // advice, not breakage: shown as [warn], never makes the server unhealthy
                    if ADVISORY_CHECKS.contains(&name) {
                        c["warning"] = json!(true);
                    }
                    ch.push(c);
                }
            }
            Err(_) if client_only::ext(&mut c) == Ok(client_only::Ext::Absent) => {
                let mut i = check("backups (pgbx extension)", true, client_only::OFF, "");
                i["info"] = json!(client_only::turn_on());
                ch.push(i);
            }
            Err(e) => ch.push(check("server checks", false, e,
                "make sure pgbx is in shared_preload_libraries (the worker creates the extension in the admin database)")),
        }
        // through an adapter the data directory is on the server, not on this machine
        if let Some(d) = c.query_one("SHOW data_directory", &[]).ok().filter(|_| !cx.conn.is_adapter()).and_then(|r| r.get::<_, Option<String>>(0)) {
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
    if cx.conn.is_adapter() {
        let mut i = check("host-side checks", true, "skipped: this profile reaches Postgres through an adapter", "");
        i["info"] = json!(host_side("doctor"));
        ch.push(i);
    }
    let healthy = ch.iter().all(|c| c["ok"] == true || c["warning"] == true);
    let mut v = json!({"ok": healthy, "healthy": healthy, "postgres_up": pg_up, "checks": ch});
    if !pg_up && !cx.conn.is_adapter() {
        let d = run_diagnose(cx, false);
        v["checks"].as_array_mut().unwrap().insert(1, check("why postgres is down", false,
            format!("{} ({}); {} evidence line(s)", scalar(&d["probable_cause"]), scalar(&d["postgres"]),
                d["evidence"].as_array().map(|a| a.len()).unwrap_or(0)),
            "see `diagnosis.steps` (pgbx diagnose): each step has a safety tier; destructive ones need human approval"));
        v["diagnosis"] = d;
    }
    Ok(v)
}

/// Host-side work (disk, logs, config files) is not reachable through an adapter: no remote exec in v1 (ADR 0003).
fn host_side(cmd: &str) -> String {
    format!("run `pgbx {cmd}` on the database host: disk, log and config checks need the host itself, \
             and this profile reaches Postgres through an adapter (pgbx runs nothing remotely)")
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
        s.push_str(&format!("[{}] {}: {}\n", if c.get("info").is_some() { "info" } else if ok { " ok " } else if c["warning"] == true { "warn" } else { "FAIL" }, scalar(&c["name"]), scalar(&c["detail"])));
        if !ok {
            s.push_str(&format!("       fix: {}\n", scalar(&c["fix"])));
        } else if let Some(i) = c.get("info") {
            s.push_str(&format!("       {}\n", scalar(i)));
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
    // one rustls crypto provider for the whole process (the build has both aws-lc-rs and ring; rustls panics on
    // the first HTTPS/TLS connection unless one is chosen): S3 over https, Postgres TLS, IMDS/STS
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let raw: Vec<String> = std::env::args().skip(1).collect();
    // credentials_file aws-default: AWS_ACCESS_KEY_ID & co. of the user running pgbx count (never in the extension)
    s3auth::allow_env_keys();
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
    // archive_command / restore_command: the exit code is the protocol (no JSON, no profile, no tunnel)
    if a.cmd == "wal-push" {
        std::process::exit(pitrcmd::wal_push_main(&a));
    }
    if a.cmd == "wal-get" {
        std::process::exit(pitrcmd::wal_get_main(&a));
    }
    let mut a = a;
    // `pgbx setup --pitr` (0.6 pre-release spelling) is `pgbx setup pitr`
    if a.cmd == "setup" && a.has("pitr") && a.pos.is_empty() {
        a.pos.push("pitr".into());
    }
    let cmd = a.cmd.clone();
    let lvl = level(&cmd, &a);
    let setup_sub = if cmd == "setup" { a.pos.first().cloned() } else { None };
    let sel = if cmd == "profile" || setup_sub.as_deref() == Some("client") {
        Ok(Arc::new(conn::Conn::direct(None, None, None)))
    } else {
        profile::select_sys(&mut a).map(|(c, notes)| {
            for n in notes {
                eprintln!("pgbx: {n}");
            }
            c
        })
    };
    let prof = sel.as_ref().ok().and_then(|c| c.profile().map(String::from));
    let mut cx = Ctx::new(a, sel.clone().unwrap_or_else(|_| Arc::new(conn::Conn::direct(None, None, None))));
    // host-side commands read this machine's files: with an adapter profile the database is elsewhere
    let host_cmd = cmd == "diagnose" || (cmd == "setup" && setup_sub.as_deref() != Some("client"));
    let r = match cmd.as_str() {
        _ if sel.is_err() => Err(sel.as_ref().err().cloned().unwrap_or_default()),
        _ if host_cmd && cx.conn.is_adapter() => Err(format!("`pgbx {cmd}` is host-side: {}", host_side(&cmd))),
        "profile" => profile::run_sys(&cx.a),
        "memories" => memories::run_sys(&cx.a, Some(cx.conn.memory_name())),
        "query" => query::run(&mut cx),
        "status" => cmd_status(&mut cx),
        "list" => cmd_list(&mut cx),
        "now" => cmd_now(&mut cx),
        "verify" => cmd_verify(&mut cx),
        "backups" => cmd_backups(&mut cx),
        "db-restore" => cmd_db_restore(&mut cx),
        "doctor" => cmd_doctor(&mut cx),
        "logs" => cmd_logs(&mut cx),
        "jobs" => jobs::run(&mut cx),
        "load" => load::run(&mut cx),
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
        "serve" => serve::run(&mut cx),
        "metrics" => metrics::cmd(&mut cx),
        "decrypt" => decrypt::cmd(&mut cx),
        "pitr" => pitrcmd::pitr(&mut cx),
        "setup" => match setup_sub.as_deref() {
            Some("client") => setup_client::run(&mut cx),
            Some("pitr") => pitrcmd::setup_pitr(&mut cx),
            Some("server") | None => setup::run(&mut cx),
            Some(x) => Err(format!("unknown setup '{x}' (pgbx setup server | pgbx setup client | pgbx setup pitr)")),
        },
        _ => unreachable!(),
    };
    let mut v = r.unwrap_or_else(|e| json!({"ok": false, "error": client_only::friendly(&cmd, e)}));
    adapter::stop_all(); // a one-off command's adapter stops when the command is done
    v["command"] = json!(cmd);
    v["safety"] = json!(format!("{lvl:?}").to_lowercase());
    if cmd == "setup" && setup_sub.is_none() {
        v["hint"] = json!("same as pgbx setup server");
        if !as_json {
            eprintln!("pgbx setup: same as pgbx setup server");
        }
    }
    if let Some(p) = &prof {
        v["profile_used"] = json!(p);
    }
    let v = vars::scrub_value(&v); // no password or expanded secret ever reaches output
    let ok = v["ok"] == true;
    if as_json && !(cmd == "decrypt" && ok && v["out"].is_null()) {
        println!("{v}"); // (decrypt without --out: stdout carries the plaintext dump)
    } else if cmd == "decrypt" && ok {
        eprintln!("pgbx decrypt: {} bytes decrypted", v["bytes"]);
    } else if cmd == "metrics" && ok {
        print!("{}", scalar(&v["text"]));
    } else if cmd == "link" && ok {
        println!("{}", scalar(&v["url"])); // bare URL so URL=$(pgbx link) works
    } else if cmd == "doctor" && v.get("checks").is_some() {
        print!("{}", doctor_text(&v));
        if v.get("diagnosis").is_some() {
            print!("\n{}", diagnose::text(&v["diagnosis"]));
        }
    } else if cmd == "load" && ok && v.get("sample").is_some() {
        print!("{}", load::text(&v));
    } else if cmd == "jobs" && ok && v.get("jobs").is_some() {
        print!("{}", jobs::text(&v));
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
    adapter::exit(if ok { 0 } else { 1 });
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
        assert_eq!(level("jobs", &p(&["jobs"])), Level::ReadOnly);
        assert_eq!(level("load", &p(&["load"])), Level::ReadOnly);
        assert_eq!(level("load", &p(&["load", "--gate", "on", "--db", "x"])), Level::Guarded);
        assert_eq!(level("load", &p(&["load", "--gate", "shadow", "--db", "x"])), Level::Safe);
        assert_eq!(level("schedule", &p(&["schedule", "suggest"])), Level::ReadOnly);
        assert_eq!(level("schedule", &p(&["schedule", "suggest", "--apply"])), Level::Safe);
        assert_eq!(level("jobs", &p(&["jobs", "cancel", "7"])), Level::Guarded);
        assert_eq!(level("serve", &p(&["serve", "--no-open"])), Level::ReadOnly);
        assert_eq!(level("serve", &p(&["serve", "--allow-safe"])), Level::Safe);
    }

    #[test]
    fn extras_flags_and_levels() {
        let a = p(&["db-restore", "--db", "shop", "--into", "s2", "--with-roles", "--roles", "all", "--key-file", "/k"]);
        assert!(a.has("with-roles") && a.get("roles") == Some("all") && a.get("key-file") == Some("/k"));
        assert_eq!(check_roles_scope(None).unwrap(), "referenced");
        assert_eq!(check_roles_scope(Some("all")).unwrap(), "all");
        assert!(check_roles_scope(Some("everyone")).is_err());
        assert_eq!(level("metrics", &p(&["metrics"])), Level::ReadOnly);
        assert_eq!(level("decrypt", &p(&["decrypt", "--key-file", "k"])), Level::ReadOnly);
        assert_eq!(level("retention", &p(&["retention", "--gfs", "7d,4w,12m"])), Level::Guarded);
        assert!(parse_args(["mcp"].iter().map(|s| s.to_string())).is_err(), "no MCP server");
    }

    #[test]
    fn pitr_commands_and_levels() {
        assert_eq!(level("pitr", &p(&["pitr", "status"])), Level::ReadOnly);
        assert_eq!(level("pitr", &p(&["pitr", "list", "--conf", "/c"])), Level::ReadOnly);
        assert_eq!(level("pitr", &p(&["pitr", "backup-now"])), Level::Safe);
        assert_eq!(level("pitr", &p(&["pitr", "restore", "--time", "latest", "--target", "/d"])), Level::Safe);
        assert_eq!(level("pitr", &p(&["pitr", "restore", "--target", "/d", "--yes-replace-whole-server"])), Level::Destructive);
        assert_eq!(level("pitr", &p(&["pitr", "expire"])), Level::Guarded);
        assert_eq!(level("setup", &p(&["setup", "pitr"])), Level::Guarded);
        assert_eq!(level("setup", &p(&["setup", "--pitr"])), Level::Guarded);
        assert_eq!(level("wal-push", &p(&["wal-push", "pg_wal/x"])), Level::Safe);
        let a = p(&["wal-get", "000000010000000000000003", "pg_wal/RECOVERYXLOG", "--conf", "/c"]);
        assert_eq!((a.pos.len(), a.get("conf")), (2, Some("/c")));
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
