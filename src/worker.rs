//! The background worker. Postgres starts it (shared_preload_libraries), restarts it if it dies, stops it on shutdown.
//! It talks to each database as a normal client over the local socket, so one worker can serve every database.

use crate::*;
use chrono::{DateTime, Utc};
use pgrx::bgworkers::{BackgroundWorker, SignalWakeFlags};
use postgres::{Client, NoTls};
use s3::{Bucket, Region, creds::Credentials};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

pub(crate) const VERIFY_PREFIX: &str = "pgbx_verify_";

pub(crate) fn setting(g: &GucSetting<Option<CString>>) -> Option<String> {
    g.get().map(|c| c.to_string_lossy().into_owned()).filter(|s| !s.is_empty())
}

/// Postgres client errors print as just "db error"; surface the server's actual message.
pub(crate) fn pe<E: std::error::Error + 'static>(e: E) -> String {
    let any: &(dyn std::error::Error + 'static) = &e;
    match any.downcast_ref::<postgres::Error>().and_then(|p| p.as_db_error()) {
        Some(d) => format!("{}{}", d.message(), d.detail().map(|x| format!(" ({x})")).unwrap_or_default()),
        None => e.to_string(),
    }
}

static STOPPING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// True once Postgres asked the worker to stop. Sticky (pgrx's flag resets on read), so every long step —
/// child processes, upload parts, retry waits — can bail out within a second and never block a shutdown.
pub(crate) fn shutting_down() -> bool {
    use std::sync::atomic::Ordering;
    if STOPPING.load(Ordering::Relaxed) {
        return true;
    }
    if unsafe { !pg_sys::MyBgworkerEntry.is_null() } && BackgroundWorker::sigterm_received() {
        STOPPING.store(true, Ordering::Relaxed);
        return true;
    }
    false
}

pub(crate) fn log(msg: &str) {
    pgrx::log!("pgbx: {msg}");
}

#[pg_guard]
#[unsafe(no_mangle)]
pub extern "C-unwind" fn pgbx_worker_main(_arg: pg_sys::Datum) {
    BackgroundWorker::attach_signal_handlers(SignalWakeFlags::SIGHUP | SignalWakeFlags::SIGTERM);
    log("worker started");
    loop {
        if BackgroundWorker::sighup_received() {
            unsafe { pg_sys::ProcessConfigFile(pg_sys::GucContext::PGC_SIGHUP) };
            log("settings reloaded");
        }
        if let Err(e) = tick() {
            log(&format!("tick failed: {e}"));
        }
        if shutting_down() {
            break;
        }
        let wait = Duration::from_secs(POLL_SECONDS.get().max(1) as u64);
        if !BackgroundWorker::wait_latch(Some(wait)) || shutting_down() {
            break;
        }
    }
    log("worker stopping");
}

pub(crate) struct Ctx {
    pub socket: String,
    pub port: i32,
    pub bindir: PathBuf,
    pub server: String,
}

fn ctx() -> Ctx {
    let exec = unsafe { std::ffi::CStr::from_ptr(std::ptr::addr_of!(pg_sys::my_exec_path).cast::<std::ffi::c_char>()) }
        .to_string_lossy()
        .into_owned();
    let bindir = PathBuf::from(exec).parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let host = std::fs::read_to_string("/etc/hostname").unwrap_or_else(|_| "postgres".into());
    Ctx {
        socket: setting(&SOCKET_DIR).unwrap_or("/var/run/postgresql".into()),
        port: unsafe { pg_sys::PostPortNumber },
        bindir,
        server: setting(&SERVER_NAME).unwrap_or_else(|| host.trim().to_string()),
    }
}

pub(crate) fn connect(c: &Ctx, db: &str) -> Result<Client, String> {
    Client::configure()
        .host_path(&c.socket)
        .port(c.port as u16)
        .user("postgres")
        .dbname(db)
        .application_name("pgbx")
        .connect(NoTls)
        .map_err(|e| format!("connect {db}: {}", pe(e)))
}

/// (access_key_id, secret_access_key) from pgbx.credentials_file
pub(crate) fn credentials() -> Result<(String, String), String> {
    let file = setting(&CREDENTIALS_FILE).ok_or("pgbx.credentials_file is not set")?;
    let text = std::fs::read_to_string(&file).map_err(|e| format!("read {file}: {e}"))?;
    let get = |k: &str| text.lines().find_map(|l| l.trim().strip_prefix(k).map(|v| v.trim_start_matches('=').trim().to_string()));
    Ok((get("access_key_id").ok_or("access_key_id missing")?, get("secret_access_key").ok_or("secret_access_key missing")?))
}

pub(crate) fn bucket() -> Result<Box<Bucket>, String> {
    let name = setting(&S3_BUCKET).ok_or("pgbx.s3_bucket is not set")?;
    let endpoint = setting(&S3_ENDPOINT).ok_or("pgbx.s3_endpoint is not set")?;
    let region = setting(&S3_REGION).unwrap_or("us-east-1".into());
    let (ak, sk) = credentials()?;
    let creds = Credentials::new(Some(&ak), Some(&sk), None, None, None).map_err(pe)?;
    let b = Bucket::new(&name, Region::Custom { region, endpoint }, creds).map_err(pe)?;
    Ok(b.with_path_style())
}

/// One pass over every database.
fn tick() -> Result<(), String> {
    let c = ctx();
    ensure_template1(&c);
    let admin_db = setting(&ADMIN_DB).unwrap_or("postgres".into());
    let mut admin = connect(&c, &admin_db)?;
    admin.batch_execute("CREATE EXTENSION IF NOT EXISTS pgbx").map_err(pe)?;
    drop_stale_verify_dbs(&mut admin);
    {
        use std::sync::atomic::{AtomicBool, Ordering};
        static CLEANED: AtomicBool = AtomicBool::new(false);
        if !CLEANED.swap(true, Ordering::Relaxed) {
            reset_our_archive_command(&mut admin);
            if let Ok(b) = bucket() {
                transfer::abort_orphans(&b, &format!("{}/", c.server));
            }
        }
    }
    let dbs: Vec<String> = admin
        .query(
            "SELECT datname FROM pg_database WHERE datallowconn AND NOT datistemplate AND datname NOT LIKE $1 ORDER BY datname",
            &[&format!("{VERIFY_PREFIX}%")],
        )
        .map_err(pe)?
        .iter()
        .map(|r| r.get(0))
        .collect();
    for db in &dbs {
        if let Err(e) = serve_db(&c, &mut admin, db) {
            log(&format!("{db}: {e}"));
            let _ = admin.execute(
                "INSERT INTO pgbx.server_overview (database, state, last_error, seen_at) VALUES ($1, 'worker error', $2, now())
                 ON CONFLICT (database) DO UPDATE SET state = 'worker error', last_error = EXCLUDED.last_error, seen_at = now()",
                &[db, &e],
            );
        }
    }
    let _ = admin.execute("DELETE FROM pgbx.server_overview WHERE NOT (database::text = ANY($1))", &[&dbs]);
    Ok(())
}

/// Is `cmd` exactly the archive_command older pgbx versions (whole-server backups) wrote:
/// `pgbackrest --config=<work_dir>/pgbackrest.conf --stanza=main archive-push %p`? Anything else is not ours.
pub(crate) fn is_our_archive_command(cmd: &str) -> bool {
    cmd.trim()
        .strip_prefix("pgbackrest --config=")
        .and_then(|r| r.strip_suffix(" --stanza=main archive-push %p"))
        .is_some_and(|conf| conf.starts_with('/') && conf.ends_with("/pgbackrest.conf") && !conf.contains(char::is_whitespace))
}

/// Whole-server backups are gone: undo the archive_command an older version set (only that one), so Postgres
/// does not keep calling a pgBackRest that is no longer configured. archive_mode is left alone (needs a restart).
fn reset_our_archive_command(admin: &mut Client) {
    // not SHOW: with archive_mode=off it prints "(disabled)"; read what postgresql(.auto).conf actually sets
    let Ok(row) = admin.query_one(
        "SELECT coalesce((SELECT setting FROM pg_file_settings WHERE name = 'archive_command' AND error IS NULL
                           ORDER BY seqno DESC LIMIT 1), '')",
        &[],
    ) else { return };
    let cmd: String = row.get(0);
    if !is_our_archive_command(&cmd) {
        return;
    }
    // two calls: a multi-statement string runs as one implicit transaction, and ALTER SYSTEM refuses that
    match admin.batch_execute("ALTER SYSTEM RESET archive_command").and_then(|_| admin.batch_execute("SELECT pg_reload_conf()")) {
        Ok(()) => log(&format!("reset archive_command (was set by an older version for whole-server backups: {cmd})")),
        Err(e) => log(&format!("could not reset archive_command: {}", pe(e))),
    }
}

/// "0.5.0" < "0.10.0": compare dotted numeric versions; anything unparsable compares as equal (never update).
pub(crate) fn version_older(installed: &str, available: &str) -> bool {
    let p = |v: &str| v.split('.').map(|x| x.parse::<u64>().ok()).collect::<Option<Vec<_>>>();
    match (p(installed), p(available)) {
        (Some(a), Some(b)) => a < b,
        _ => false,
    }
}

/// After a binary upgrade the library is new but each database still has the old SQL objects:
/// run ALTER EXTENSION ... UPDATE when the installed version is older than the library's default one.
fn update_extension(cl: &mut Client, db: &str) -> Result<(), String> {
    let row = cl
        .query_opt(
            "SELECT e.extversion, a.default_version FROM pg_extension e JOIN pg_available_extensions a ON a.name = e.extname
             WHERE e.extname = 'pgbx'",
            &[],
        )
        .map_err(pe)?;
    let Some(row) = row else { return Ok(()) };
    let (have, want): (String, Option<String>) = (row.get(0), row.get(1));
    if let Some(want) = want.filter(|w| version_older(&have, w)) {
        cl.batch_execute("ALTER EXTENSION pgbx UPDATE").map_err(|e| format!("{db}: ALTER EXTENSION UPDATE: {}", pe(e)))?;
        log(&format!("{db}: updated extension {have} -> {want}"));
    }
    Ok(())
}

/// Put pgbx in template1 so every new database is born with it. Postgres refuses
/// CREATE DATABASE while anyone is connected to the template, so connect rarely (startup, then every
/// 10 minutes) and close at once — never hold a template1 session.
fn ensure_template1(c: &Ctx) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = Utc::now().timestamp() as u64;
    if now.saturating_sub(LAST.load(Ordering::Relaxed)) < 600 {
        return;
    }
    let r = connect(c, "template1").and_then(|mut t1| {
        t1.batch_execute("CREATE EXTENSION IF NOT EXISTS pgbx").map_err(|e| format!("template1: {}", pe(e)))?;
        update_extension(&mut t1, "template1") // so new databases are born at the current version
    }); // connection dropped here
    match r {
        Ok(()) => LAST.store(now, Ordering::Relaxed),
        Err(e) => log(&e),
    }
}

/// Scratch databases from a verify that was cut off by a crash.
fn drop_stale_verify_dbs(admin: &mut Client) {
    if let Ok(rows) = admin.query("SELECT datname FROM pg_database WHERE datname LIKE $1", &[&format!("{VERIFY_PREFIX}%")]) {
        for r in rows {
            let name: String = r.get(0);
            let _ = admin.batch_execute(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"));
        }
    }
}

/// Is a job of this kind due? `last` = when the previous automatic one was requested.
pub(crate) fn is_due(cron: &str, last: Option<DateTime<Utc>>) -> Result<bool, String> {
    match last {
        None => Ok(true),
        Some(t) => Ok(schedule::next_after(cron, t)? <= Utc::now()),
    }
}

pub(crate) fn last_auto(cl: &mut Client, kind: &str) -> Result<Option<DateTime<Utc>>, String> {
    Ok(cl
        .query_one(
            "SELECT max(requested_at) FROM pgbx.history WHERE kind=$1 AND trigger IN ('schedule','first')",
            &[&kind],
        )
        .map_err(pe)?
        .get::<_, Option<std::time::SystemTime>>(0)
        .map(DateTime::<Utc>::from))
}

pub(crate) fn pending(cl: &mut Client, kind: &str) -> Result<i64, String> {
    Ok(cl
        .query_one("SELECT count(*) FROM pgbx.history WHERE kind=$1 AND state IN ('queued','running')", &[&kind])
        .map_err(pe)?
        .get(0))
}

fn serve_db(c: &Ctx, admin: &mut Client, db: &str) -> Result<(), String> {
    let mut cl = connect(c, db)?;
    // ensure the policy row exists; jobs still 'running' here were cut off by a restart or copied in by a restore
    cl.batch_execute(
        "CREATE EXTENSION IF NOT EXISTS pgbx;",
    )
    .map_err(pe)?;
    update_extension(&mut cl, db)?;
    cl.batch_execute(
        "INSERT INTO pgbx.config DEFAULT VALUES ON CONFLICT (id) DO NOTHING;
         UPDATE pgbx.history SET state='failed', finished=now(), error='interrupted (worker restart or copied by restore)'
          WHERE state='running';",
    )
    .map_err(pe)?;
    let row = cl
        .query_one(
            "SELECT coalesce(path, current_database()), schedule, max_backups, max_days, enabled, verify_schedule FROM pgbx.config",
            &[],
        )
        .map_err(pe)?;
    let (path, schedule, max_backups, max_days, enabled, verify_cron): (String, String, i32, i32, bool, Option<String>) =
        (row.get(0), row.get(1), row.get(2), row.get(3), row.get(4), row.get(5));
    let max_days = max_days.min(MAX_DAYS_LIMIT.get()); // server-wide ceiling wins

    if enabled {
        let last = last_auto(&mut cl, "backup")?;
        if pending(&mut cl, "backup")? == 0 && is_due(&schedule, last)? {
            let trigger = if last.is_none() { "first" } else { "schedule" };
            cl.execute("INSERT INTO pgbx.history (kind, trigger) VALUES ('backup', $1)", &[&trigger]).map_err(pe)?;
        }
        // restore tests follow their own schedule, and only once there is a backup to test
        if let Some(v) = verify_cron.as_deref() {
            let has_backup: bool = cl
                .query_one("SELECT EXISTS (SELECT 1 FROM pgbx.history WHERE kind='backup' AND state='done')", &[])
                .map_err(pe)?
                .get(0);
            // first restore test waits for the schedule (no 'first' trigger): measure from the first backup
            let last_v = last_auto(&mut cl, "verify")?.or(cl
                .query_one("SELECT min(finished) FROM pgbx.history WHERE kind='backup' AND state='done'", &[])
                .map_err(pe)?
                .get::<_, Option<std::time::SystemTime>>(0)
                .map(DateTime::<Utc>::from));
            if has_backup && pending(&mut cl, "verify")? == 0 && is_due(v, last_v)? {
                cl.execute("INSERT INTO pgbx.history (kind, trigger) VALUES ('verify', 'schedule')", &[]).map_err(pe)?;
            }
        }
    }

    // run queued jobs, oldest first; a deferred one waits for its deferred_until
    let jobs = cl
        .query(
            "SELECT id, kind, params::text, trigger, requested_at FROM pgbx.history WHERE state='queued'
               AND coalesce((params->>'deferred_until')::timestamptz, '-infinity') <= now() ORDER BY id",
            &[],
        )
        .map_err(pe)?;
    for j in jobs {
        let (id, kind, params, trigger): (i64, String, String, String) = (j.get(0), j.get(1), j.get(2), j.get(3));
        let requested = DateTime::<Utc>::from(j.get::<_, std::time::SystemTime>(4));
        let deadline = defer_deadline(requested, trigger == "first", &schedule);
        let deferred = parse_flat_json(&params).contains_key("deferrals");
        let forced = kind == "backup" && deferred && Utc::now() >= deadline;
        cl.execute(
            "UPDATE pgbx.history SET state='running', started=now(),
                    params = CASE WHEN $2 THEN params || '{\"forced\":true}' ELSE params END WHERE id=$1",
            &[&id, &forced],
        )
        .map_err(pe)?;
        let res: Result<Done, String> = match kind.as_str() {
            "backup" => backup(c, db, &path, forced),
            "restore" => restore(c, admin, &path, &params),
            "verify" => verify(c, admin, &mut cl, &path),
            "prune" => Ok(Done::default()),
            other => Err(format!("unknown job kind {other}")),
        };
        match res {
            Ok(done) => {
                // record the new backup first so prune sees it as a live row, then expire, then mark the job done
                if kind == "backup" {
                    cl.execute("UPDATE pgbx.history SET s3_key=$2, bytes=$3 WHERE id=$1", &[&id, &done.key, &done.bytes])
                        .map_err(pe)?;
                }
                if kind == "backup" || kind == "prune" {
                    match prune(c, &path, max_backups, max_days) {
                        Ok(gone) if !gone.is_empty() => {
                            cl.execute("UPDATE pgbx.history SET state='expired' WHERE kind='backup' AND s3_key = ANY($1)", &[&gone])
                                .map_err(pe)?;
                            log(&format!("{db}: expired {} old backup(s)", gone.len()));
                        }
                        Ok(_) => {}
                        Err(e) => log(&format!("{db}: prune failed: {e}")),
                    }
                }
                cl.execute(
                    "UPDATE pgbx.history SET state='done', finished=now(), s3_key=$2, bytes=$3,
                            params = params || $4::text::jsonb WHERE id=$1",
                    &[&id, &done.key, &done.bytes, &done.extra],
                )
                .map_err(pe)?;
                log(&format!("{db}: {kind} #{id} done ({}, {} bytes)", done.key.as_deref().unwrap_or("-"), done.bytes));
            }
            // a lock held by DDL/a migration: never queue behind it; try again later, until the deadline
            Err(e) if kind == "backup" && !forced && is_lock_timeout(&e) => {
                let p = parse_flat_json(&params);
                let n = |k: &str| p.get(k).and_then(|v| v.parse::<i32>().ok()).unwrap_or(0);
                let (deferrals, locks) = (n("deferrals"), n("lock_timeouts"));
                let wait = backoff_minutes(setting(&DEFER_BACKOFF).as_deref(), deferrals as usize);
                let until = (Utc::now() + chrono::Duration::minutes(wait as i64)).min(deadline);
                cl.execute(
                    "UPDATE pgbx.history SET state='queued', started=NULL, params = params || jsonb_build_object(
                        'deferrals', $2::int, 'lock_timeouts', $3::int, 'deferred_until', $4::timestamptz,
                        'defer_reason', 'lock_timeout', 'deadline', $5::timestamptz) WHERE id=$1",
                    &[&id, &(deferrals + 1), &(locks + 1), &std::time::SystemTime::from(until), &std::time::SystemTime::from(deadline)],
                )
                .map_err(pe)?;
                log(&format!(
                    "{db}: backup #{id} could not get its table locks within pgbx.dump_lock_timeout (DDL holds one); retry at {}, runs anyway from {}",
                    until.format("%H:%M:%S"), deadline.format("%Y-%m-%d %H:%M:%S UTC")
                ));
            }
            Err(e) => {
                cl.execute("UPDATE pgbx.history SET state='failed', finished=now(), error=$2 WHERE id=$1", &[&id, &e])
                    .map_err(pe)?;
                log(&format!("{db}: {kind} #{id} failed: {e}"));
                alert(&c.server, db, &kind, id, &e);
            }
        }
    }
    prune_audit(&mut cl, db);
    publish_overview(admin, &mut cl, db)
}

/// Audit retention (pgbx.audit_days): at most once an hour per database.
fn prune_audit(cl: &mut Client, db: &str) {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::Instant;
    static LAST: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);
    {
        let mut g = LAST.lock().unwrap_or_else(|e| e.into_inner());
        let m = g.get_or_insert_with(HashMap::new);
        if m.get(db).is_some_and(|t| t.elapsed() < Duration::from_secs(3600)) {
            return;
        }
        m.insert(db.to_string(), Instant::now());
    }
    match crate::audit::prune(cl, crate::AUDIT_DAYS.get()) {
        Ok(0) => {}
        Ok(n) => log(&format!("{db}: pruned {n} history row(s) older than pgbx.audit_days")),
        Err(e) => log(&format!("{db}: audit prune failed: {e}")),
    }
}

/// Copy this database's status() into the admin database's server_overview.
fn publish_overview(admin: &mut Client, cl: &mut Client, db: &str) -> Result<(), String> {
    let r = cl
        .query_one(
            "SELECT state, schedule, last_backup_at, last_backup_size, next_backup_at, backups_kept, last_verify_result, last_error
             FROM pgbx.status()",
            &[],
        )
        .map_err(pe)?;
    let (state, schedule, last_at, size, next_at, kept, verify, err): (
        Option<String>, Option<String>, Option<std::time::SystemTime>, Option<String>, Option<std::time::SystemTime>,
        Option<i64>, Option<String>, Option<String>,
    ) = (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4), r.get(5), r.get(6), r.get(7));
    admin
        .execute(
            "INSERT INTO pgbx.server_overview AS o
                (database, state, schedule, last_backup_at, last_backup_size, next_backup_at, backups_kept, last_verify, last_error, seen_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9, now())
             ON CONFLICT (database) DO UPDATE SET state=$2, schedule=$3, last_backup_at=$4, last_backup_size=$5,
                next_backup_at=$6, backups_kept=$7, last_verify=$8, last_error=$9, seen_at=now()",
            &[&db, &state, &schedule, &last_at, &size, &next_at, &kept, &verify, &err],
        )
        .map_err(pe)?;
    Ok(())
}

/// What a finished job reports back into its history row.
struct Done {
    key: Option<String>,
    bytes: i64,
    extra: String, // JSON object merged into params; always a valid object ("{}" when there is nothing to add)
}

impl Default for Done {
    fn default() -> Self {
        Done { key: None, bytes: 0, extra: "{}".into() }
    }
}

/// Run pgbx.alert_command for a failed job: JSON on stdin, PGBX_* env vars, 30 s limit.
pub(crate) fn alert(server: &str, db: &str, kind: &str, id: i64, error: &str) {
    let Some(cmd) = setting(&ALERT_COMMAND) else { return };
    let json = format!(
        "{{\"server\":{},\"database\":{},\"kind\":{},\"job_id\":{id},\"error\":{},\"at\":\"{}\"}}\n",
        jstr(server), jstr(db), jstr(kind), jstr(error), Utc::now().to_rfc3339()
    );
    let child = Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .env("PGBX_SERVER", server)
        .env("PGBX_DATABASE", db)
        .env("PGBX_KIND", kind)
        .env("PGBX_JOB_ID", id.to_string())
        .env("PGBX_ERROR", error)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return log("alert_command could not start") };
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(json.as_bytes());
    }
    for _ in 0..300 {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    log("alert_command timed out after 30s");
}

fn jstr(s: &str) -> String {
    let mut o = String::from("\"");
    for ch in s.chars() {
        match ch {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn prefix(c: &Ctx, path: &str) -> String {
    format!("{}/{}/", c.server, path)
}

fn user_tables(cl: &mut Client) -> Result<i64, String> {
    Ok(cl
        .query_one(
            "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
             WHERE c.relkind IN ('r','p') AND n.nspname NOT IN ('pg_catalog','information_schema','pgbx')
               AND n.nspname NOT LIKE 'pg_toast%'",
            &[],
        )
        .map_err(pe)?
        .get(0))
}

/// pg_dump (custom format) streamed straight into a multipart S3 upload — no temp file, no full copy in memory.
/// Key: s3://bucket/<server>/<path>/<UTC timestamp>.dump. Records how many user tables it saw (for verify).
fn backup(c: &Ctx, db: &str, path: &str, forced: bool) -> Result<Done, String> {
    let mut src = connect(c, db)?;
    let tables = user_tables(&mut src)?;
    // data scope: definitions of every table are dumped; rows are skipped for these (resolved now, so new tables count)
    let rowless: Vec<String> = src.query("SELECT table_name FROM pgbx.rowless_tables()", &[]).map_err(pe)?
        .iter().map(|r| r.get(0)).collect();
    drop(src);
    let b = bucket()?;
    let key = format!("{}{}.dump", prefix(c, path), Utc::now().format("%Y-%m-%dT%H-%M-%SZ"));
    let (pg_dump, major) = client_tool(c, "pg_dump");
    let normal = compression(setting(&DUMP_COMPRESSION).as_deref(), major);
    let compress = if forced { compression_busy(setting(&DUMP_COMPRESSION_BUSY).as_deref(), &normal, major) } else { normal };
    // pg_dump SETs lock_timeout=0 itself, so its own --lock-wait-timeout is the knob; C messages so a timeout is recognised
    let lock_ms = if forced { DUMP_LOCK_TIMEOUT_FORCED.get() } else { DUMP_LOCK_TIMEOUT.get() };
    let mut child = job_command(&pg_dump, "pgbx_dump")
        .env("PGOPTIONS", "-c lc_messages=C")
        .args(["-Fc", "--compress", &compress, "-h", &c.socket, "-p", &c.port.to_string(), "-U", "postgres", "-d", db])
        .args((lock_ms > 0).then(|| format!("--lock-wait-timeout={lock_ms}ms")))
        .args(rowless.iter().map(|t| format!("--exclude-table-data={t}")))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn pg_dump: {e}"))?;
    let mut out = child.stdout.take().unwrap();
    let up = transfer::upload_stream(&b, &key, &mut out);
    drop(out); // if the upload gave up, this stops pg_dump (broken pipe)
    if up.is_err() {
        let _ = child.kill();
    }
    let res = child.wait_with_output().map_err(pe)?;
    if !res.status.success() {
        let _ = b.delete_object(&key); // never leave a truncated dump that looks like a backup
        let why = up.err().unwrap_or_else(|| String::from_utf8_lossy(&res.stderr).trim().to_string());
        return Err(format!("backup failed: {why}"));
    }
    let bytes = up?;
    let skipped = rowless.iter().map(|t| jstr(t)).collect::<Vec<_>>().join(",");
    Ok(Done {
        key: Some(key),
        bytes: bytes as i64,
        extra: format!("{{\"tables\":{tables},\"compression\":\"{compress}\",\"rows_skipped\":[{skipped}]}}"),
    })
}

/// Major version from `pg_dump --version` output ("pg_dump (PostgreSQL) 16.4 (Debian ...)" -> 16).
pub(crate) fn parse_major(out: &str) -> Option<u32> {
    let v = out.split(')').nth(1)?.split_whitespace().next()?;
    v.split(|ch: char| !ch.is_ascii_digit()).next()?.parse().ok()
}

/// pgbx.dump_compression: 'auto' (or unset) = zstd:3 when pg_dump is 16+, else gzip level 6
/// (pg_dump before 16 takes only a 0-9 level).
pub(crate) fn compression(setting: Option<&str>, pg_dump_major: Option<u32>) -> String {
    match setting.map(str::trim) {
        Some(s) if !s.is_empty() && !s.eq_ignore_ascii_case("auto") => s.to_string(),
        _ if pg_dump_major.unwrap_or(0) >= 16 => "zstd:3".into(),
        _ => "6".into(),
    }
}

/// pgbx.dump_compression_busy: 'auto' (or unset) = zstd:1 when pg_dump is 16+, else gzip level 1. Single-threaded
/// either way (no zstd workers=): one core at most. 'auto' never costs more than `normal` (none / 0 / lz4 stay).
pub(crate) fn compression_busy(setting: Option<&str>, normal: &str, pg_dump_major: Option<u32>) -> String {
    let n = normal.trim().to_ascii_lowercase();
    match setting.map(str::trim) {
        Some(s) if !s.is_empty() && !s.eq_ignore_ascii_case("auto") => s.to_string(),
        _ if matches!(n.as_str(), "none" | "0" | "gzip:0") || n.starts_with("lz4") => normal.to_string(),
        _ if pg_dump_major.unwrap_or(0) >= 16 => "zstd:1".into(),
        _ => "1".into(),
    }
}

/// Did pg_dump give up waiting for a table lock (--lock-wait-timeout)? It runs the LOCK TABLEs under a
/// statement_timeout; with NOWAIT the server says "could not obtain lock".
pub(crate) fn is_lock_timeout(err: &str) -> bool {
    err.contains("LOCK TABLE")
        && (err.contains("statement timeout") || err.contains("lock timeout") || err.contains("could not obtain lock"))
}

/// pgbx.defer_backoff ("1,2,4,8,15", minutes, each 1-60): wait before retry number `n` (0-based); the last value
/// repeats. A list that does not parse falls back to the default.
pub(crate) fn backoff_minutes(setting: Option<&str>, n: usize) -> u32 {
    const DEFAULT: [u32; 5] = [1, 2, 4, 8, 15];
    let parsed: Option<Vec<u32>> = setting.map(|s| {
        s.split(',').map(|x| x.trim().parse::<u32>().ok().filter(|m| (1..=60).contains(m))).collect::<Option<Vec<_>>>()
    })
    .flatten()
    .filter(|v| !v.is_empty());
    let list = parsed.as_deref().unwrap_or(&DEFAULT);
    list[n.min(list.len() - 1)]
}

/// When a deferred job runs anyway: queued + pgbx.max_defer (first backup: pgbx.max_defer_first), never later than
/// one schedule interval, so a deferred backup and the next scheduled one never pile up.
pub(crate) fn defer_deadline(requested: DateTime<Utc>, first: bool, cron: &str) -> DateTime<Utc> {
    let max = if first { MAX_DEFER_FIRST.get() } else { MAX_DEFER.get() };
    requested + chrono::Duration::seconds(defer_secs(max as i64, schedule_interval(cron, requested)))
}

fn defer_secs(max_defer: i64, interval: Option<i64>) -> i64 {
    interval.map_or(max_defer, |i| max_defer.min(i)).max(0)
}

/// Seconds between the two schedule slots after `at` (None when the schedule cannot be read).
pub(crate) fn schedule_interval(cron: &str, at: DateTime<Utc>) -> Option<i64> {
    let a = schedule::next_after(cron, at).ok()?;
    let b = schedule::next_after(cron, a).ok()?;
    Some((b - a).num_seconds())
}

/// pgbx.job_ionice -> the ioprio_set value: None = leave alone ('none'), else class << 13 | level
/// ('idle' = class 3; 'best-effort-N' = class 2, level N 0-7).
pub(crate) fn parse_ionice(s: &str) -> Result<Option<i32>, String> {
    const BE: i32 = 2 << 13;
    const IDLE: i32 = 3 << 13;
    match s.trim().to_ascii_lowercase().as_str() {
        "none" => Ok(None),
        "idle" => Ok(Some(IDLE)),
        v => v
            .strip_prefix("best-effort-")
            .and_then(|n| n.parse::<i32>().ok())
            .filter(|n| (0..=7).contains(n))
            .map(|n| Some(BE | n))
            .ok_or(format!("pgbx.job_ionice '{s}' not understood (none, idle, best-effort-0..7); using best-effort-7")),
    }
}

/// pg_dump / pg_restore as a polite child: lower CPU priority (pgbx.job_nice) and, on Linux, IO priority
/// (pgbx.job_ionice), set between fork and exec so they hold from its first read. PGAPPNAME tags its connection
/// so load checks skip it and doctor() can see how long it runs. Never raises priority.
fn job_command(prog: &std::path::Path, app: &str) -> Command {
    let mut cmd = Command::new(prog);
    cmd.env("PGAPPNAME", app);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let nice = JOB_NICE.get().clamp(0, 19);
        let ioprio = parse_ionice(setting(&JOB_IONICE).as_deref().unwrap_or("best-effort-7")).unwrap_or_else(|e| {
            log(&e);
            Some((2 << 13) | 7)
        });
        let current = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
        // SAFETY: only async-signal-safe syscalls between fork and exec; failures are ignored (best effort)
        unsafe {
            cmd.pre_exec(move || {
                if nice > current {
                    libc::setpriority(libc::PRIO_PROCESS, 0, nice);
                }
                #[cfg(target_os = "linux")]
                if let Some(p) = ioprio {
                    libc::syscall(libc::SYS_ioprio_set, 1 /* IOPRIO_WHO_PROCESS */, 0, p);
                }
                #[cfg(not(target_os = "linux"))]
                let _ = ioprio;
                Ok(())
            });
        }
    }
    cmd
}

/// The newest installed copy of a client tool (pg_dump / pg_restore): newer clients dump and restore older
/// servers, so prefer e.g. /usr/lib/postgresql/18/bin over the server's own bindir. Returns (path, major).
pub(crate) fn client_tool(c: &Ctx, name: &str) -> (PathBuf, Option<u32>) {
    let mut cands = vec![c.bindir.join(name)];
    for root in ["/usr/lib/postgresql", "/usr"] {
        if let Ok(rd) = std::fs::read_dir(root) {
            for e in rd.flatten() {
                let f = e.file_name().to_string_lossy().into_owned();
                if root == "/usr/lib/postgresql" || f.starts_with("pgsql-") {
                    cands.push(e.path().join("bin").join(name));
                }
            }
        }
    }
    let mut best: (PathBuf, Option<u32>) = (c.bindir.join(name), None);
    for p in cands.into_iter().filter(|p| p.is_file()) {
        let major = Command::new(&p).arg("--version").output().ok().and_then(|o| parse_major(&String::from_utf8_lossy(&o.stdout)));
        if major.is_some() && major > best.1 {
            best = (p, major);
        }
    }
    best
}

fn list_dumps(c: &Ctx, b: &Bucket, path: &str) -> Result<Vec<String>, String> {
    let mut keys: Vec<String> = b
        .list(prefix(c, path), None)
        .map_err(|e| format!("list: {e}"))?
        .into_iter()
        .flat_map(|page| page.contents.into_iter().map(|o| o.key))
        .filter(|k| k.ends_with(".dump"))
        .collect();
    keys.sort(); // timestamp names sort chronologically
    Ok(keys)
}

fn key_time(key: &str) -> Option<DateTime<Utc>> {
    let stem = key.rsplit('/').next()?.strip_suffix(".dump")?;
    chrono::NaiveDateTime::parse_from_str(stem, "%Y-%m-%dT%H-%M-%SZ").ok().map(|n| n.and_utc())
}

/// Delete backups beyond the newest `max_backups`, and any older than `max_days`; never the newest one.
/// Returns the deleted keys.
fn prune(c: &Ctx, path: &str, max_backups: i32, max_days: i32) -> Result<Vec<String>, String> {
    let b = bucket()?;
    let keys = list_dumps(c, &b, path)?; // oldest first
    let cutoff = Utc::now() - chrono::Duration::days(max_days as i64);
    let keep_from = keys.len().saturating_sub(max_backups.max(1) as usize);
    let mut gone = Vec::new();
    for (i, k) in keys.iter().enumerate() {
        if i + 1 == keys.len() {
            break; // newest: always kept
        }
        let too_many = i < keep_from;
        let too_old = key_time(k).is_some_and(|t| t < cutoff);
        if too_many || too_old {
            b.delete_object(k).map_err(|e| format!("delete {k}: {e}"))?;
            gone.push(k.clone());
        }
    }
    Ok(gone)
}

fn pick_backup(c: &Ctx, b: &Bucket, path: &str, at: DateTime<Utc>) -> Result<String, String> {
    list_dumps(c, b, path)?
        .into_iter()
        .filter(|k| key_time(k).is_some_and(|t| t <= at))
        .next_back()
        .ok_or(format!("no backup of '{path}' at or before {at}"))
}

/// CREATE DATABASE <into> TEMPLATE template0, then stream the S3 object straight into pg_restore's stdin.
fn restore_key_into(c: &Ctx, admin: &mut Client, b: &Bucket, key: &str, into: &str, app: &str) -> Result<i64, String> {
    // template0: the dump brings its own CREATE EXTENSION pgbx, so start from a database without it
    admin
        .batch_execute(&format!("CREATE DATABASE \"{into}\" TEMPLATE template0"))
        .map_err(|e| format!("create {into}: {}", pe(e)))?;
    // the target is a NEW database: losing the tail of it in a crash just means restoring again
    let sync = if RESTORE_SYNCHRONOUS_COMMIT.get() { "on" } else { "off" };
    let mut child = job_command(&client_tool(c, "pg_restore").0, app)
        .env("PGOPTIONS", format!("-c synchronous_commit={sync}"))
        .args(["-h", &c.socket, "-p", &c.port.to_string(), "-U", "postgres", "--no-owner", "-d", into])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn pg_restore: {e}"))?;
    let mut stdin = child.stdin.take().unwrap();
    let dl = transfer::download_resumable(b, key, &mut stdin);
    drop(stdin); // EOF for pg_restore
    if dl.is_err() && shutting_down() {
        let _ = child.kill();
    }
    let out = child.wait_with_output().map_err(pe)?;
    if !out.status.success() {
        return Err(format!("pg_restore failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let n = dl?;
    Ok(n as i64)
}

/// Newest dump at or before `at` -> a NEW database. The live one is never touched.
fn restore(c: &Ctx, admin: &mut Client, path: &str, params: &str) -> Result<Done, String> {
    let p = parse_flat_json(params);
    let into = p.get("into_db").ok_or("restore needs into_db")?.clone();
    if !into.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
        return Err(format!("into_db '{into}' must be [A-Za-z0-9_]"));
    }
    let at = p
        .get("at")
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc)))
        .unwrap_or_else(Utc::now);
    let b = bucket()?;
    let key = pick_backup(c, &b, path, at)?;
    let bytes = restore_key_into(c, admin, &b, &key, &into, "pgbx_restore")?;
    // the copy must not back up into the original's folder: give it its own path (= its own name)
    let mut copy = connect(c, &into)?;
    copy.batch_execute("UPDATE pgbx.config SET path = NULL").map_err(pe)?;
    Ok(Done { key: Some(key), bytes, extra: "{}".into() })
}

/// Restore test: newest backup -> scratch database -> same number of user tables as at backup time -> drop.
fn verify(c: &Ctx, admin: &mut Client, source: &mut Client, path: &str) -> Result<Done, String> {
    let b = bucket()?;
    let key = pick_backup(c, &b, path, Utc::now())?;
    let scratch = format!("{VERIFY_PREFIX}{}", Utc::now().format("%Y%m%d%H%M%S"));
    let result = (|| {
        let bytes = restore_key_into(c, admin, &b, &key, &scratch, "pgbx_verify")?;
        let mut sc = connect(c, &scratch)?;
        let got = user_tables(&mut sc)?;
        let ok = sc.query_one("SELECT 1", &[]).is_ok();
        drop(sc);
        // compare with what the backup job counted when it took this dump (recorded in the source database)
        let want: Option<i64> = source
            .query_opt("SELECT (params->>'tables')::bigint FROM pgbx.history WHERE s3_key=$1 AND kind='backup'", &[&key])
            .map_err(pe)?
            .and_then(|r| r.get(0));
        match want {
            Some(w) if w != got => Err(format!("restored {got} tables, backup had {w}")),
            _ if !ok => Err("restored database does not answer queries".into()),
            _ => Ok(Done {
                key: Some(key.clone()),
                bytes,
                extra: format!("{{\"checked\":\"restored {got} tables from {}\"}}", key.rsplit('/').next().unwrap_or("")),
            }),
        }
    })();
    let _ = admin.batch_execute(&format!("DROP DATABASE IF EXISTS \"{scratch}\" WITH (FORCE)"));
    result
}

/// Tiny parser for the flat {"k": "v"} objects jsonb renders (values are strings, numbers, null or timestamps).
pub(crate) fn parse_flat_json(s: &str) -> std::collections::HashMap<String, String> {
    let mut m = std::collections::HashMap::new();
    for part in s.trim().trim_start_matches('{').trim_end_matches('}').split(',') {
        if let Some((k, v)) = part.split_once(':') {
            let (k, v) = (k.trim().trim_matches('"'), v.trim());
            if v != "null" && !k.is_empty() {
                m.insert(k.to_string(), v.trim_matches('"').to_string());
            }
        }
    }
    m
}

#[cfg(test)]
mod t {
    use super::*;

    #[test]
    fn our_archive_command_only() {
        assert!(is_our_archive_command("pgbackrest --config=/var/lib/postgresql/pgbx/pgbackrest.conf --stanza=main archive-push %p"));
        assert!(!is_our_archive_command("pgbackrest --config=/etc/pgbackrest/pgbackrest.conf --stanza=prod archive-push %p"));
        assert!(!is_our_archive_command("pgbackrest --stanza=main archive-push %p"));
        assert!(!is_our_archive_command("cp %p /mnt/archive/%f"));
        assert!(!is_our_archive_command(""));
        assert!(!is_our_archive_command("pgbackrest --config=/x/pgbackrest.conf --stanza=main archive-push %p && rm -rf /"));
    }

    #[test]
    fn versions() {
        assert!(version_older("0.4.0", "0.5.0"));
        assert!(version_older("0.9.0", "0.10.0"));
        assert!(!version_older("0.5.0", "0.5.0"));
        assert!(!version_older("0.6.0", "0.5.0"));
        assert!(!version_older("dev", "0.5.0"));
    }

    #[test]
    fn compression_by_pg_dump_version() {
        assert_eq!(parse_major("pg_dump (PostgreSQL) 16.4 (Debian 16.4-1.pgdg120+1)\n"), Some(16));
        assert_eq!(parse_major("pg_restore (PostgreSQL) 13.15\n"), Some(13));
        assert_eq!(parse_major("pg_dump (PostgreSQL) 18beta1"), Some(18));
        assert_eq!(compression(Some("auto"), Some(16)), "zstd:3");
        assert_eq!(compression(None, Some(13)), "6");
        assert_eq!(compression(Some("auto"), None), "6");
        assert_eq!(compression(Some("lz4"), Some(13)), "lz4");
    }

    #[test]
    fn prune_job_extra_is_valid_json() {
        assert_eq!(Done::default().extra, "{}");
    }

    #[test]
    fn ionice_values() {
        assert_eq!(parse_ionice("best-effort-7"), Ok(Some((2 << 13) | 7)));
        assert_eq!(parse_ionice(" Best-Effort-0 "), Ok(Some(2 << 13)));
        assert_eq!(parse_ionice("idle"), Ok(Some(3 << 13)));
        assert_eq!(parse_ionice("none"), Ok(None));
        for bad in ["best-effort-8", "best-effort--1", "best-effort", "realtime-0", "", "7"] {
            assert!(parse_ionice(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn busy_compression_is_cheap() {
        assert_eq!(compression_busy(Some("auto"), "zstd:3", Some(16)), "zstd:1");
        assert_eq!(compression_busy(None, "6", Some(15)), "1");
        assert_eq!(compression_busy(Some("lz4"), "zstd:3", Some(16)), "lz4");
        // auto never makes a forced run more expensive than the normal setting
        for cheap in ["none", "0", "lz4", "lz4:1"] {
            assert_eq!(compression_busy(Some("auto"), cheap, Some(16)), cheap);
        }
    }

    #[test]
    fn lock_timeout_detected() {
        // what pg_dump --lock-wait-timeout prints when a table is held ACCESS EXCLUSIVE (lc_messages=C)
        let e = "backup failed: pg_dump: error: query failed: ERROR:  canceling statement due to statement timeout\n\
                 pg_dump: detail: Query was: LOCK TABLE public.t IN ACCESS SHARE MODE";
        assert!(is_lock_timeout(e));
        assert!(is_lock_timeout("ERROR:  could not obtain lock on relation \"t\"\nQuery was: LOCK TABLE public.t IN ACCESS SHARE MODE NOWAIT"));
        assert!(!is_lock_timeout("backup failed: pg_dump: error: connection to server failed"));
        assert!(!is_lock_timeout("ERROR:  canceling statement due to statement timeout\nQuery was: SELECT 1"));
    }

    #[test]
    fn backoff_schedule() {
        let b = |s: Option<&str>| (0..7).map(|n| backoff_minutes(s, n)).collect::<Vec<_>>();
        assert_eq!(b(None), [1, 2, 4, 8, 15, 15, 15]);
        assert_eq!(b(Some("1,2,4,8,15")), [1, 2, 4, 8, 15, 15, 15]);
        assert_eq!(b(Some(" 5 , 10 ")), [5, 10, 10, 10, 10, 10, 10]);
        assert_eq!(b(Some("0,5")), [1, 2, 4, 8, 15, 15, 15]); // each 1-60, else the default
        assert_eq!(b(Some("61")), [1, 2, 4, 8, 15, 15, 15]);
        assert_eq!(b(Some("")), [1, 2, 4, 8, 15, 15, 15]);
    }

    #[test]
    fn deadline_capped_by_schedule_interval() {
        let at = DateTime::parse_from_rfc3339("2026-10-02T02:00:00Z").unwrap().with_timezone(&Utc);
        assert_eq!(schedule_interval("0 2 * * *", at), Some(86_400));
        assert_eq!(schedule_interval("0 * * * *", at), Some(3_600));
        assert_eq!(schedule_interval("not cron", at), None);
        assert_eq!(defer_secs(4 * 3600, Some(86_400)), 4 * 3600); // daily: 4h
        assert_eq!(defer_secs(4 * 3600, Some(3_600)), 3_600); // hourly: never past the next slot
        assert_eq!(defer_secs(900, None), 900);
        assert_eq!(defer_secs(0, Some(3_600)), 0);
    }

    #[test]
    fn update_script_matches_install() {
        // each function the update script replaces must read exactly as the install script creates it
        let lib = include_str!("lib.rs");
        let upd = include_str!("../sql/pgbx--0.5.0--0.6.0.sql");
        let fns: Vec<&str> = upd.split("CREATE OR REPLACE FUNCTION ").skip(1).collect();
        assert!(!fns.is_empty());
        for f in fns {
            let body = &f[..f.find("END $$;").expect("plpgsql body")];
            assert!(lib.contains(&format!("CREATE FUNCTION {body}")), "update script differs from lib.rs: {}", &body[..40]);
        }
    }
}
