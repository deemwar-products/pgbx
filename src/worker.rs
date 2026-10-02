//! The background worker. Postgres starts it (shared_preload_libraries), restarts it if it dies, stops it on shutdown.
//! It talks to each database as a normal client over the local socket, so one worker can serve every database.
//!
//! One server-wide job queue (ADR 0001 §0): every poll the worker collects the queued jobs of every database, picks
//! by priority, then age, then round-robin over databases, and starts each picked job on its own thread (which runs
//! pg_dump / pg_restore as a child process). The main loop never blocks on a job: it supervises them every second,
//! keeps polling, and answers SIGTERM. Each running job holds one advisory-lock slot in the admin database, so no
//! more than pgbx.max_concurrent_jobs run at once, even with a second worker.
//!
//! Job threads never call into Postgres (no elog, no GUC reads): settings are snapshotted on the main thread
//! (`JobCfg`), log lines are queued and written by the main thread, and they talk to databases as ordinary clients.

use crate::*;
use chrono::{DateTime, Utc};
use pgrx::bgworkers::{BackgroundWorker, SignalWakeFlags};
use postgres::{Client, NoTls};
use s3::{Bucket, Region, creds::Credentials};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Instant, SystemTime};

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

static STOPPING: AtomicBool = AtomicBool::new(false);
static MAIN_THREAD: OnceLock<std::thread::ThreadId> = OnceLock::new();
static LOG_QUEUE: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// What the main thread knows about one running job, shared with that job's thread.
#[derive(Default)]
pub(crate) struct JobCtl {
    stop: AtomicBool,      // cancel or shutdown: every long step bails out
    cancelled: AtomicBool, // ... because pgbx.cancel() asked
    pid: AtomicI32,        // the pg_dump / pg_restore child, 0 when none
}

thread_local! {
    static JOB: RefCell<Option<Arc<JobCtl>>> = const { RefCell::new(None) };
}

fn on_main_thread() -> bool {
    MAIN_THREAD.get().is_none_or(|t| *t == std::thread::current().id())
}

fn this_job() -> Option<Arc<JobCtl>> {
    JOB.with(|j| j.borrow().clone())
}

/// True once Postgres asked the worker to stop, or (on a job thread) once that job was cancelled. Sticky (pgrx's
/// flag resets on read), so every long step — child processes, upload parts, retry waits — can bail out within a
/// second and never block a shutdown.
pub(crate) fn shutting_down() -> bool {
    if STOPPING.load(Ordering::Relaxed) {
        return true;
    }
    if this_job().is_some_and(|j| j.stop.load(Ordering::Relaxed)) {
        return true;
    }
    if on_main_thread() && unsafe { !pg_sys::MyBgworkerEntry.is_null() } && BackgroundWorker::sigterm_received() {
        STOPPING.store(true, Ordering::Relaxed);
        return true;
    }
    false
}

/// Why shutting_down() is true, for error messages.
pub(crate) fn stop_reason() -> &'static str {
    if this_job().is_some_and(|j| j.cancelled.load(Ordering::Relaxed)) { "cancelled" } else { "Postgres is shutting down" }
}

/// Remember the job's child process, so the main thread can stop it on cancel or shutdown.
fn set_child(pid: u32) {
    if let Some(j) = this_job() {
        j.pid.store(pid as i32, Ordering::Relaxed);
    }
}

/// The Postgres log, from any thread: job threads queue their lines, the main thread writes them.
pub(crate) fn log(msg: &str) {
    if on_main_thread() {
        pgrx::log!("pgbx: {msg}");
    } else {
        LOG_QUEUE.lock().unwrap_or_else(|e| e.into_inner()).push(msg.to_string());
    }
}

fn drain_logs() {
    let lines = std::mem::take(&mut *LOG_QUEUE.lock().unwrap_or_else(|e| e.into_inner()));
    for l in lines {
        pgrx::log!("pgbx: {l}");
    }
}

#[pg_guard]
#[unsafe(no_mangle)]
pub extern "C-unwind" fn pgbx_worker_main(_arg: pg_sys::Datum) {
    BackgroundWorker::attach_signal_handlers(SignalWakeFlags::SIGHUP | SignalWakeFlags::SIGTERM);
    let _ = MAIN_THREAD.set(std::thread::current().id());
    log("worker started");
    let mut s = Sched::default();
    let mut next_tick = Instant::now();
    loop {
        if BackgroundWorker::sighup_received() {
            unsafe { pg_sys::ProcessConfigFile(pg_sys::GucContext::PGC_SIGHUP) };
            log("settings reloaded");
        }
        drain_logs();
        // a job that ended frees its slot: look for the next one at once
        if reap(&mut s) || Instant::now() >= next_tick {
            if let Err(e) = tick(&mut s) {
                log(&format!("tick failed: {e}"));
            }
            next_tick = Instant::now() + Duration::from_secs(POLL_SECONDS.get().max(1) as u64);
        }
        drain_logs();
        if shutting_down() {
            break;
        }
        // supervise running jobs every second; otherwise sleep until the next poll
        let left = next_tick.saturating_duration_since(Instant::now());
        let wait = if s.running.is_empty() { left } else { left.min(Duration::from_secs(1)) };
        if !BackgroundWorker::wait_latch(Some(wait.max(Duration::from_millis(50)))) || shutting_down() {
            break;
        }
    }
    stop_all(&mut s);
    log("worker stopping");
}

/// Shutdown: stop every job (their threads abort uploads and record the failure), wait up to 20 s for them.
fn stop_all(s: &mut Sched) {
    STOPPING.store(true, Ordering::Relaxed);
    for r in &s.running {
        r.ctl.stop.store(true, Ordering::Relaxed);
        kill_child(&r.ctl);
    }
    let t0 = Instant::now();
    while !s.running.is_empty() && t0.elapsed() < Duration::from_secs(20) {
        reap(s);
        drain_logs();
        std::thread::sleep(Duration::from_millis(100));
    }
    drain_logs();
}

fn kill_child(ctl: &JobCtl) {
    let pid = ctl.pid.load(Ordering::Relaxed);
    if pid > 0 {
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
}

#[derive(Clone)]
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

/// Every setting a job needs, read on the main thread when the job starts (GUCs must not be read elsewhere).
#[derive(Clone)]
pub(crate) struct JobCfg {
    bucket: Result<Box<Bucket>, String>,
    compression: Option<String>,
    compression_busy: Option<String>,
    lock_ms: i32,
    lock_ms_forced: i32,
    nice: i32,
    ionice: Option<String>,
    restore_sync: bool,
    upload_kbps: i32,
    download_kbps: i32,
    alert_command: Option<String>,
    defer_backoff: Option<String>,
    max_defer: i32,
    max_defer_first: i32,
    admin_db: String,
}

impl JobCfg {
    fn now() -> JobCfg {
        JobCfg {
            bucket: bucket(),
            compression: setting(&DUMP_COMPRESSION),
            compression_busy: setting(&DUMP_COMPRESSION_BUSY),
            lock_ms: DUMP_LOCK_TIMEOUT.get(),
            lock_ms_forced: DUMP_LOCK_TIMEOUT_FORCED.get(),
            nice: JOB_NICE.get(),
            ionice: setting(&JOB_IONICE),
            restore_sync: RESTORE_SYNCHRONOUS_COMMIT.get(),
            upload_kbps: UPLOAD_KBPS.get(),
            download_kbps: DOWNLOAD_KBPS.get(),
            alert_command: setting(&ALERT_COMMAND),
            defer_backoff: setting(&DEFER_BACKOFF),
            max_defer: MAX_DEFER.get(),
            max_defer_first: MAX_DEFER_FIRST.get(),
            admin_db: setting(&ADMIN_DB).unwrap_or("postgres".into()),
        }
    }

    fn bucket(&self) -> Result<Box<Bucket>, String> {
        self.bucket.clone()
    }
}

/// A job on its thread, as the main loop sees it.
struct Running {
    db: String,
    id: i64,
    kind: String,
    trigger: String,
    requested: DateTime<Utc>,
    started: DateTime<Utc>,
    slot: i32, // advisory-lock slot 1..max_concurrent_jobs; 0 = the restore lane
    owns_db: Option<String>, // the NEW database a restore / restore test writes into: not served while it runs
    ctl: Arc<JobCtl>,
    handle: Option<std::thread::JoinHandle<()>>,
}

/// The worker's state across polls.
#[derive(Default)]
struct Sched {
    running: Vec<Running>,
    last_start: HashMap<String, Instant>, // round-robin: the database that waited longest goes first on ties
}

/// A queued job of some database, as found this poll.
struct Cand {
    db: String,
    id: i64,
    kind: String,
    trigger: String,
    params: String,
    requested: DateTime<Utc>,
    path: String,
    schedule: String,
    max_backups: i32,
    max_days: i32,
}

/// Pick order (ADR 0001 §0): restore (a human waits) > manual backup > scheduled / first backup > verify > prune.
pub(crate) fn priority(kind: &str, trigger: &str, params: &str) -> u8 {
    match kind {
        "restore" => 0,
        "backup" if trigger == "manual" || parse_flat_json(params).get("manual").is_some_and(|v| v == "true") => 1,
        "backup" => 2,
        "verify" => 3,
        "prune" => 4,
        _ => 5,
    }
}

/// Collect finished job threads; true when any ended.
fn reap(s: &mut Sched) -> bool {
    let mut any = false;
    let mut i = 0;
    while i < s.running.len() {
        if s.running[i].handle.as_ref().is_none_or(|h| h.is_finished()) {
            let mut r = s.running.remove(i);
            if let Some(h) = r.handle.take()
                && h.join().is_err()
            {
                log(&format!("{}: {} #{}: job thread panicked", r.db, r.kind, r.id));
            }
            any = true;
        } else {
            i += 1;
        }
    }
    any
}

/// One pass over every database: queue due jobs, collect what is queued server-wide, start what fits.
fn tick(s: &mut Sched) -> Result<(), String> {
    let c = ctx();
    ensure_template1(&c);
    let admin_db = setting(&ADMIN_DB).unwrap_or("postgres".into());
    let mut admin = connect(&c, &admin_db)?;
    admin.batch_execute("CREATE EXTENSION IF NOT EXISTS pgbx").map_err(pe)?;
    let owned: Vec<String> = s.running.iter().filter_map(|r| r.owns_db.clone()).collect();
    drop_stale_verify_dbs(&mut admin, &owned);
    {
        use std::sync::atomic::AtomicBool;
        static CLEANED: AtomicBool = AtomicBool::new(false);
        // once per worker start, before any job runs: uploads a crash cut off are never a backup
        if s.running.is_empty()
            && !CLEANED.swap(true, Ordering::Relaxed)
            && let Ok(b) = bucket()
        {
            transfer::abort_orphans(&b, &format!("{}/", c.server));
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
    let (mut cands, mut deferred) = (Vec::new(), Vec::new());
    let mut conns: HashMap<String, Client> = HashMap::new();
    for db in &dbs {
        if owned.contains(db) {
            continue; // a restore is still writing it: it is not a live database yet
        }
        match scan_db(&c, &mut admin, db, s) {
            Ok((cl, mut found, mut later)) => {
                cands.append(&mut found);
                deferred.append(&mut later);
                conns.insert(db.clone(), cl);
            }
            Err(e) => {
                log(&format!("{db}: {e}"));
                let _ = admin.execute(
                    "INSERT INTO pgbx.server_overview (database, state, last_error, seen_at) VALUES ($1, 'worker error', $2, now())
                     ON CONFLICT (database) DO UPDATE SET state = 'worker error', last_error = EXCLUDED.last_error, seen_at = now()",
                    &[db, &e],
                );
            }
        }
    }
    let _ = admin.execute("DELETE FROM pgbx.server_overview WHERE NOT (database::text = ANY($1))", &[&dbs]);
    let waiting = start_jobs(&c, &admin_db, s, cands, &mut conns);
    publish_queue(&mut admin, s, &waiting, &deferred, &mut conns);
    Ok(())
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
    use std::sync::atomic::AtomicU64;
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

/// Scratch databases from a restore test that was cut off by a crash (never one a running test still uses).
fn drop_stale_verify_dbs(admin: &mut Client, owned: &[String]) {
    if let Ok(rows) = admin.query("SELECT datname FROM pg_database WHERE datname LIKE $1", &[&format!("{VERIFY_PREFIX}%")]) {
        for r in rows {
            let name: String = r.get(0);
            if !owned.contains(&name) {
                let _ = admin.batch_execute(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"));
            }
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

/// Whether the next scheduled backup is due, and how many slots a long dump made it skip.
#[derive(Debug, PartialEq)]
pub(crate) enum Due {
    No,
    Yes { skipped: u32, guard: bool },
}

/// pgbx.overrun_policy (ADR 0001 §0.5). `last_auto`: when the last schedule/first backup was requested;
/// `last_run`: (started, finished) of the newest finished backup of any trigger; `last_good`: when the newest
/// successful backup finished.
/// skip: slots that passed while a dump was running are dropped; the next run is the next slot after that dump
/// finished — unless waiting for it would leave more than `max_gap` schedule intervals since the last good backup
/// finished, then it runs right away (`guard`). catch_up: run once right away.
pub(crate) fn due_backup(
    cron: &str, now: DateTime<Utc>, last_auto: Option<DateTime<Utc>>, last_run: Option<(DateTime<Utc>, DateTime<Utc>)>,
    last_good: Option<DateTime<Utc>>, policy: Overrun, max_gap: f64,
) -> Result<Due, String> {
    let Some(r) = last_auto else { return Ok(Due::Yes { skipped: 0, guard: false }) };
    let slot = schedule::next_after(cron, r)?;
    if slot > now {
        return Ok(Due::No);
    }
    let Some((started, finished)) = last_run.filter(|(s, f)| policy == Overrun::Skip && *s <= slot && *f >= slot) else {
        return Ok(Due::Yes { skipped: 0, guard: false });
    };
    let _ = started;
    let (mut skipped, mut next) = (0u32, slot);
    while next <= finished && skipped < 100_000 {
        skipped += 1;
        next = schedule::next_after(cron, next)?;
    }
    if next <= now {
        return Ok(Due::Yes { skipped, guard: false });
    }
    let interval = (schedule::next_after(cron, next)? - next).num_seconds() as f64;
    let gap_ok = last_good.is_some_and(|g| (next - g).num_seconds() as f64 <= max_gap * interval);
    Ok(if gap_ok { Due::No } else { Due::Yes { skipped, guard: true } })
}

pub(crate) fn last_auto(cl: &mut Client, kind: &str) -> Result<Option<DateTime<Utc>>, String> {
    Ok(cl
        .query_one(
            "SELECT max(requested_at) FROM pgbx.history WHERE kind=$1 AND trigger IN ('schedule','first')",
            &[&kind],
        )
        .map_err(pe)?
        .get::<_, Option<SystemTime>>(0)
        .map(DateTime::<Utc>::from))
}

pub(crate) fn pending(cl: &mut Client, kind: &str) -> Result<i64, String> {
    Ok(cl
        .query_one("SELECT count(*) FROM pgbx.history WHERE kind=$1 AND state IN ('queued','running')", &[&kind])
        .map_err(pe)?
        .get(0))
}

/// Queue a scheduled backup when it is due under pgbx.overrun_policy.
fn queue_scheduled_backup(cl: &mut Client, db: &str, schedule: &str) -> Result<(), String> {
    if pending(cl, "backup")? > 0 {
        return Ok(());
    }
    let last = last_auto(cl, "backup")?;
    let ts = |v: Option<SystemTime>| v.map(DateTime::<Utc>::from);
    let run = cl
        .query_opt(
            "SELECT started, finished FROM pgbx.history WHERE kind='backup' AND started IS NOT NULL AND finished IS NOT NULL
             ORDER BY finished DESC LIMIT 1",
            &[],
        )
        .map_err(pe)?
        .and_then(|r| Some((ts(r.get(0))?, ts(r.get(1))?)));
    let good = ts(cl
        .query_one("SELECT max(finished) FROM pgbx.history WHERE kind='backup' AND state IN ('done','expired')", &[])
        .map_err(pe)?
        .get(0));
    let due = due_backup(schedule, Utc::now(), last, run, good, OVERRUN_POLICY.get(), OVERRUN_MAX_GAP.get())?;
    if let Due::Yes { skipped, guard } = due {
        let trigger = if last.is_none() { "first" } else { "schedule" };
        let params = if skipped > 0 {
            format!("{{\"skipped_slots\":{skipped}{}}}", if guard { ",\"overrun_guard\":true" } else { "" })
        } else {
            "{}".into()
        };
        cl.execute("INSERT INTO pgbx.history (kind, trigger, params) VALUES ('backup', $1, $2::text::jsonb)", &[&trigger, &params])
            .map_err(pe)?;
        if skipped > 0 {
            log(&format!(
                "{db}: {skipped} schedule slot(s) passed while a dump was running (pgbx.overrun_policy = skip){}",
                if guard { "; running now, pgbx.overrun_max_gap reached" } else { "" }
            ));
        }
    }
    Ok(())
}

/// Per database, every poll: keep the extension current, recover jobs a restart cut off, pass on cancel requests,
/// queue due jobs, publish the overview row; returns the connection, this database's queued jobs that may start now,
/// and the deferred ones (with the reason).
fn scan_db(c: &Ctx, admin: &mut Client, db: &str, s: &Sched) -> Result<(Client, Vec<Cand>, Vec<(Cand, String)>), String> {
    let mut cl = connect(c, db)?;
    cl.batch_execute("CREATE EXTENSION IF NOT EXISTS pgbx;").map_err(pe)?;
    update_extension(&mut cl, db)?;
    // 'running' rows no job thread of this worker owns were cut off by a restart or copied in by a restore
    let mine: Vec<i64> = s.running.iter().filter(|r| r.db == db).map(|r| r.id).collect();
    cl.execute("INSERT INTO pgbx.config DEFAULT VALUES ON CONFLICT (id) DO NOTHING", &[]).map_err(pe)?;
    cl.execute(
        "UPDATE pgbx.history SET state='failed', finished=now(), error='interrupted (worker restart or copied by restore)'
          WHERE state='running' AND NOT (id = ANY($1))",
        &[&mine],
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

    // pgbx.cancel() of a running job: stop its thread and child; the thread records 'cancelled'
    for r in cl.query("SELECT id FROM pgbx.history WHERE state='running' AND params ? 'cancel_requested'", &[]).map_err(pe)? {
        let id: i64 = r.get(0);
        if let Some(job) = s.running.iter().find(|x| x.db == db && x.id == id)
            && !job.ctl.cancelled.swap(true, Ordering::Relaxed)
        {
            job.ctl.stop.store(true, Ordering::Relaxed);
            kill_child(&job.ctl);
            log(&format!("{db}: {} #{id} cancel requested: stopping it", job.kind));
        }
    }

    if enabled {
        queue_scheduled_backup(&mut cl, db, &schedule)?;
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
                .get::<_, Option<SystemTime>>(0)
                .map(DateTime::<Utc>::from));
            if has_backup && pending(&mut cl, "verify")? == 0 && is_due(v, last_v)? {
                cl.execute("INSERT INTO pgbx.history (kind, trigger) VALUES ('verify', 'schedule')", &[]).map_err(pe)?;
            }
        }
    }

    // queued jobs; a deferred one waits for its deferred_until
    let (mut found, mut deferred) = (Vec::new(), Vec::new());
    for j in cl
        .query(
            "SELECT id, kind, params::text, trigger, requested_at,
                    CASE WHEN (params->>'deferred_until')::timestamptz > now()
                         THEN format('deferred (%s) until %s UTC, runs anyway from %s UTC', coalesce(params->>'defer_reason', '?'),
                                     to_char((params->>'deferred_until')::timestamptz AT TIME ZONE 'UTC', 'HH24:MI:SS'),
                                     coalesce(to_char((params->>'deadline')::timestamptz AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI'), '?')) END
               FROM pgbx.history WHERE state='queued' ORDER BY id",
            &[],
        )
        .map_err(pe)?
    {
        let cand = Cand {
            db: db.to_string(),
            id: j.get(0),
            kind: j.get(1),
            params: j.get(2),
            trigger: j.get(3),
            requested: DateTime::<Utc>::from(j.get::<_, SystemTime>(4)),
            path: path.clone(),
            schedule: schedule.clone(),
            max_backups,
            max_days,
        };
        match j.get::<_, Option<String>>(5) {
            Some(why) => deferred.push((cand, why)),
            None => found.push(cand),
        }
    }
    prune_audit(&mut cl, db);
    publish_overview(admin, &mut cl, db, &schedule)?;
    Ok((cl, found, deferred))
}

/// Start queued jobs, best first, while job slots are free. A job that cannot start now (its database is
/// already being dumped) does not hold up the jobs behind it. Returns the jobs left waiting, in pick order,
/// each with the reason.
fn start_jobs(c: &Ctx, admin_db: &str, s: &mut Sched, mut cands: Vec<Cand>, conns: &mut HashMap<String, Client>) -> Vec<(Cand, String)> {
    let max = MAX_CONCURRENT_JOBS.get().clamp(1, 8);
    let lane = RESTORE_LANE.get();
    let never = Instant::now() - Duration::from_secs(365 * 86_400);
    cands.sort_by_key(|j| (priority(&j.kind, &j.trigger, &j.params), j.requested, *s.last_start.get(&j.db).unwrap_or(&never)));
    let mut waiting = Vec::new();
    for j in cands {
        let used: Vec<i32> = s.running.iter().map(|r| r.slot).collect();
        let general: Vec<i32> = (1..=max).filter(|n| !used.contains(n)).collect();
        let lane_free = lane && !used.contains(&0);
        let mut slots = general.clone();
        if j.kind == "restore" && lane_free {
            slots.push(0); // the restore lane: a restore never waits behind a long dump
        }
        // never two dumps of one database at once
        if j.kind == "backup" && s.running.iter().any(|r| r.db == j.db && r.kind == "backup") {
            waiting.push((j, "waits for the backup of this database that is running".to_string()));
            continue;
        }
        if slots.is_empty() {
            let busy: Vec<String> = s.running.iter().map(|r| format!("{} {} #{}", r.db, r.kind, r.id)).collect();
            let why = format!(
                "waits for a job slot: {} of {max} in use ({}){}",
                used.iter().filter(|n| **n > 0).count(),
                busy.join(", "),
                if j.kind == "restore" && lane { "; the restore lane is busy too" } else { "" }
            );
            waiting.push((j, why));
            continue;
        }
        let Some((slot, lock)) = take_slot(c, admin_db, &slots) else {
            waiting.push((j, "waits for a job slot: another pgbx worker holds them (pgbx.max_concurrent_jobs)".into()));
            continue;
        };
        let Some(cl) = conns.get_mut(&j.db) else { continue };
        let cfg = JobCfg::now();
        let p = parse_flat_json(&j.params);
        let deadline = defer_deadline(j.requested, j.trigger == "first", &j.schedule, &cfg);
        let forced = j.kind == "backup" && p.contains_key("deferrals") && Utc::now() >= deadline;
        // a cancel() between the scan and now wins: only a row still queued starts
        let n = cl.execute(
            "UPDATE pgbx.history SET state='running', started=now(),
                    params = (params - 'queue_position' - 'wait_reason')
                             || CASE WHEN $2 THEN '{\"forced\":true}'::jsonb ELSE '{}'::jsonb END
              WHERE id=$1 AND state='queued'",
            &[&j.id, &forced],
        );
        if !matches!(n, Ok(1)) {
            continue;
        }
        let owns_db = match j.kind.as_str() {
            "restore" => p.get("into_db").cloned(),
            "verify" => Some(format!("{VERIFY_PREFIX}{}_{}", Utc::now().format("%Y%m%d%H%M%S"), j.id)),
            _ => None,
        };
        let ctl = Arc::new(JobCtl::default());
        let job = Job {
            c: c.clone(),
            cfg,
            db: j.db.clone(),
            id: j.id,
            kind: j.kind.clone(),
            trigger: j.trigger.clone(),
            params: j.params.clone(),
            requested: j.requested,
            path: j.path.clone(),
            schedule: j.schedule.clone(),
            max_backups: j.max_backups,
            max_days: j.max_days,
            forced,
            scratch: owns_db.clone().filter(|_| j.kind == "verify"),
        };
        let thread_ctl = ctl.clone();
        let spawned = std::thread::Builder::new().name(format!("pgbx job {}", j.id)).spawn(move || {
            JOB.with(|x| *x.borrow_mut() = Some(thread_ctl));
            run_job(job, lock);
        });
        match spawned {
            Ok(h) => {
                let lane_note = if slot == 0 { "restore lane" } else { "slot" };
                log(&format!("{}: {} #{} started ({lane_note} {slot})", j.db, j.kind, j.id));
                s.last_start.insert(j.db.clone(), Instant::now());
                s.running.push(Running {
                    db: j.db,
                    id: j.id,
                    kind: j.kind,
                    trigger: j.trigger,
                    requested: j.requested,
                    started: Utc::now(),
                    slot,
                    owns_db,
                    ctl,
                    handle: Some(h),
                });
            }
            Err(e) => {
                let _ = cl.execute(
                    "UPDATE pgbx.history SET state='failed', finished=now(), error=$2 WHERE id=$1",
                    &[&j.id, &format!("could not start a job thread: {e}")],
                );
            }
        }
    }
    waiting
}

/// Mirror the server-wide queue into the admin database (pgbx.server_queue, `pgbx jobs`) and tell each waiting job
/// why it waits (history.params queue_position / wait_reason, shown by status()). Rows only change when it changed.
fn publish_queue(admin: &mut Client, s: &Sched, waiting: &[(Cand, String)], deferred: &[(Cand, String)], conns: &mut HashMap<String, Client>) {
    struct Row<'a> {
        db: &'a str,
        id: i64,
        kind: &'a str,
        trigger: &'a str,
        state: &'a str,
        position: Option<i32>,
        slot: Option<i32>,
        requested: SystemTime,
        started: Option<SystemTime>,
        detail: String,
    }
    let mut rows: Vec<Row> = Vec::new();
    for r in &s.running {
        let cancelling = r.ctl.cancelled.load(Ordering::Relaxed);
        rows.push(Row {
            db: &r.db,
            id: r.id,
            kind: &r.kind,
            trigger: &r.trigger,
            state: if cancelling { "cancelling" } else { "running" },
            position: None,
            slot: Some(r.slot),
            requested: r.requested.into(),
            started: Some(r.started.into()),
            detail: if cancelling {
                "cancel requested: stopping it".into()
            } else if r.slot == 0 {
                "running in the restore lane".into()
            } else {
                format!("running in job slot {}", r.slot)
            },
        });
    }
    for (i, (j, why)) in waiting.iter().enumerate() {
        rows.push(Row {
            db: &j.db, id: j.id, kind: &j.kind, trigger: &j.trigger, state: "queued", position: Some(i as i32 + 1), slot: None,
            requested: j.requested.into(), started: None, detail: why.clone(),
        });
    }
    for (j, why) in deferred {
        rows.push(Row {
            db: &j.db, id: j.id, kind: &j.kind, trigger: &j.trigger, state: "deferred", position: None, slot: None,
            requested: j.requested.into(), started: None, detail: why.clone(),
        });
    }
    let r = (|| -> Result<(), postgres::Error> {
        let mut tx = admin.transaction()?;
        tx.execute("DELETE FROM pgbx.server_queue", &[])?;
        for q in &rows {
            tx.execute(
                "INSERT INTO pgbx.server_queue (database, job_id, kind, trigger, state, position, slot, requested_at, started_at, detail)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
                &[&q.db, &q.id, &q.kind, &q.trigger, &q.state, &q.position, &q.slot, &q.requested, &q.started, &q.detail],
            )?;
        }
        tx.commit()
    })();
    if let Err(e) = r {
        log(&format!("server_queue: {}", pe(e)));
    }
    for q in rows.iter().filter(|q| q.state != "running" && q.state != "cancelling") {
        if let Some(cl) = conns.get_mut(q.db) {
            let _ = cl.execute(
                "UPDATE pgbx.history SET params = params || jsonb_build_object('queue_position', $2::int, 'wait_reason', $3::text)
                  WHERE id=$1 AND state='queued'
                    AND (params->'queue_position' IS DISTINCT FROM to_jsonb($2::int) OR params->>'wait_reason' IS DISTINCT FROM $3::text)",
                &[&q.id, &q.position, &q.detail],
            );
        }
    }
}

/// One free job slot, held as a session advisory lock in the admin database for as long as the returned
/// connection lives (the job thread keeps it). None when every slot is taken (by this or another worker).
fn take_slot(c: &Ctx, admin_db: &str, slots: &[i32]) -> Option<(i32, Client)> {
    let mut lc = connect(c, admin_db).map_err(|e| log(&format!("job slot: {e}"))).ok()?;
    for &n in slots {
        let got: bool = lc.query_one("SELECT pg_try_advisory_lock(hashtext('pgbx_job'), $1)", &[&n]).ok()?.get(0);
        if got {
            return Some((n, lc));
        }
    }
    None
}

/// Everything a job thread needs; nothing in here touches Postgres internals.
struct Job {
    c: Ctx,
    cfg: JobCfg,
    db: String,
    id: i64,
    kind: String,
    trigger: String,
    params: String,
    requested: DateTime<Utc>,
    path: String,
    schedule: String,
    max_backups: i32,
    max_days: i32,
    forced: bool,
    scratch: Option<String>,
}

/// A job thread: run it, record the result in its database. `_slot` holds the advisory lock until the end.
fn run_job(j: Job, _slot: Client) {
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| execute(&j)))
        .unwrap_or_else(|p| Err(format!("job panicked: {}", p.downcast_ref::<String>().cloned().or(p.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default())));
    let mut last = String::new();
    for attempt in 0..3 {
        match connect(&j.c, &j.db).and_then(|mut cl| record(&j, &mut cl, &res)) {
            Ok(()) => return,
            Err(e) => last = e,
        }
        if attempt < 2 && !STOPPING.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    log(&format!("{}: {} #{}: could not record the result ({last}); it is marked failed at the next start", j.db, j.kind, j.id));
}

fn execute(j: &Job) -> Result<Done, String> {
    match j.kind.as_str() {
        "backup" => backup(&j.c, &j.cfg, &j.db, &j.path, j.forced),
        "restore" => {
            let mut admin = connect(&j.c, &j.cfg.admin_db)?;
            restore(&j.c, &j.cfg, &mut admin, &j.path, &j.params)
        }
        "verify" => {
            let mut admin = connect(&j.c, &j.cfg.admin_db)?;
            let mut src = connect(&j.c, &j.db)?;
            let scratch = j.scratch.clone().unwrap_or_else(|| format!("{VERIFY_PREFIX}{}", j.id));
            verify(&j.c, &j.cfg, &mut admin, &mut src, &j.path, &scratch)
        }
        "prune" => Ok(Done::default()),
        other => Err(format!("unknown job kind {other}")),
    }
}

/// Write a finished job's outcome into its history row (and expire backups after a backup / prune).
fn record(j: &Job, cl: &mut Client, res: &Result<Done, String>) -> Result<(), String> {
    let (db, id, kind) = (j.db.as_str(), j.id, j.kind.as_str());
    let cancelled = this_job().is_some_and(|x| x.cancelled.load(Ordering::Relaxed));
    match res {
        // pgbx.cancel() of a running job: its upload was aborted / its partial database dropped; no alert
        Err(e) if cancelled => {
            cl.execute(
                "UPDATE pgbx.history SET state='cancelled', finished=now(),
                        error='cancelled by ' || coalesce(params->>'cancelled_by', '?') || ' while running' WHERE id=$1",
                &[&id],
            )
            .map_err(pe)?;
            log(&format!("{db}: {kind} #{id} cancelled ({e})"));
        }
        Ok(done) => {
            // record the new backup first so prune sees it as a live row, then expire, then mark the job done
            if kind == "backup" {
                cl.execute("UPDATE pgbx.history SET s3_key=$2, bytes=$3 WHERE id=$1", &[&id, &done.key, &done.bytes])
                    .map_err(pe)?;
            }
            if kind == "backup" || kind == "prune" {
                match prune(&j.c, &j.cfg, &j.path, j.max_backups, j.max_days) {
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
        Err(e) if kind == "backup" && !j.forced && is_lock_timeout(e) => {
            let p = parse_flat_json(&j.params);
            let n = |k: &str| p.get(k).and_then(|v| v.parse::<i32>().ok()).unwrap_or(0);
            let (deferrals, locks) = (n("deferrals"), n("lock_timeouts"));
            let deadline = defer_deadline(j.requested, j.trigger == "first", &j.schedule, &j.cfg);
            let wait = backoff_minutes(j.cfg.defer_backoff.as_deref(), deferrals as usize);
            let until = (Utc::now() + chrono::Duration::minutes(wait as i64)).min(deadline);
            cl.execute(
                "UPDATE pgbx.history SET state='queued', started=NULL, params = params || jsonb_build_object(
                    'deferrals', $2::int, 'lock_timeouts', $3::int, 'deferred_until', $4::timestamptz,
                    'defer_reason', 'lock_timeout', 'deadline', $5::timestamptz) WHERE id=$1",
                &[&id, &(deferrals + 1), &(locks + 1), &SystemTime::from(until), &SystemTime::from(deadline)],
            )
            .map_err(pe)?;
            log(&format!(
                "{db}: backup #{id} could not get its table locks within pgbx.dump_lock_timeout (DDL holds one); retry at {}, runs anyway from {}",
                until.format("%H:%M:%S"), deadline.format("%Y-%m-%d %H:%M:%S UTC")
            ));
        }
        Err(e) => {
            cl.execute("UPDATE pgbx.history SET state='failed', finished=now(), error=$2 WHERE id=$1", &[&id, e])
                .map_err(pe)?;
            log(&format!("{db}: {kind} #{id} failed: {e}"));
            alert(j.cfg.alert_command.as_deref(), &j.c.server, db, kind, id, e);
        }
    }
    Ok(())
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

/// Copy this database's status() into the admin database's server_overview (plus what doctor() needs to judge the
/// schedule: its interval and how long the last 3 backups took).
fn publish_overview(admin: &mut Client, cl: &mut Client, db: &str, cron: &str) -> Result<(), String> {
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
    let interval = schedule_interval(cron, Utc::now()).map(|s| s as f64);
    let dump_secs: Vec<f64> = cl
        .query(
            "SELECT extract(epoch FROM finished - started)::float8 FROM pgbx.history
              WHERE kind='backup' AND state IN ('done','expired') AND started IS NOT NULL ORDER BY id DESC LIMIT 3",
            &[],
        )
        .map_err(pe)?
        .iter()
        .map(|r| r.get(0))
        .collect();
    admin
        .execute(
            "INSERT INTO pgbx.server_overview AS o
                (database, state, schedule, last_backup_at, last_backup_size, next_backup_at, backups_kept, last_verify, last_error,
                 seen_at, interval_secs, dump_secs)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9, now(), $10, $11)
             ON CONFLICT (database) DO UPDATE SET state=$2, schedule=$3, last_backup_at=$4, last_backup_size=$5,
                next_backup_at=$6, backups_kept=$7, last_verify=$8, last_error=$9, seen_at=now(), interval_secs=$10, dump_secs=$11",
            &[&db, &state, &schedule, &last_at, &size, &next_at, &kept, &verify, &err, &interval, &dump_secs],
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
pub(crate) fn alert(cmd: Option<&str>, server: &str, db: &str, kind: &str, id: i64, error: &str) {
    let Some(cmd) = cmd else { return };
    let json = format!(
        "{{\"server\":{},\"database\":{},\"kind\":{},\"job_id\":{id},\"error\":{},\"at\":\"{}\"}}\n",
        jstr(server), jstr(db), jstr(kind), jstr(error), Utc::now().to_rfc3339()
    );
    let child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
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
fn backup(c: &Ctx, cfg: &JobCfg, db: &str, path: &str, forced: bool) -> Result<Done, String> {
    let mut src = connect(c, db)?;
    let tables = user_tables(&mut src)?;
    // data scope: definitions of every table are dumped; rows are skipped for these (resolved now, so new tables count)
    let rowless: Vec<String> = src.query("SELECT table_name FROM pgbx.rowless_tables()", &[]).map_err(pe)?
        .iter().map(|r| r.get(0)).collect();
    drop(src);
    let b = cfg.bucket()?;
    let key = format!("{}{}.dump", prefix(c, path), Utc::now().format("%Y-%m-%dT%H-%M-%SZ"));
    let (pg_dump, major) = client_tool(c, "pg_dump");
    let normal = compression(cfg.compression.as_deref(), major);
    let compress = if forced { compression_busy(cfg.compression_busy.as_deref(), &normal, major) } else { normal };
    // pg_dump SETs lock_timeout=0 itself, so its own --lock-wait-timeout is the knob; C messages so a timeout is recognised
    let lock_ms = if forced { cfg.lock_ms_forced } else { cfg.lock_ms };
    let mut child = job_command(&pg_dump, "pgbx_dump", cfg)
        .env("PGOPTIONS", "-c lc_messages=C")
        .args(["-Fc", "--compress", &compress, "-h", &c.socket, "-p", &c.port.to_string(), "-U", "postgres", "-d", db])
        .args((lock_ms > 0).then(|| format!("--lock-wait-timeout={lock_ms}ms")))
        .args(rowless.iter().map(|t| format!("--exclude-table-data={t}")))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn pg_dump: {e}"))?;
    set_child(child.id());
    let mut out = child.stdout.take().unwrap();
    let up = transfer::upload_stream(&b, &key, &mut out, cfg.upload_kbps);
    drop(out); // if the upload gave up, this stops pg_dump (broken pipe)
    if up.is_err() {
        let _ = child.kill();
    }
    let res = child.wait_with_output().map_err(pe);
    set_child(0);
    let res = res?;
    if !res.status.success() || (up.is_ok() && shutting_down()) {
        let _ = b.delete_object(&key); // never leave a truncated (or cancelled) dump that looks like a backup
        let why = up.err().unwrap_or_else(|| {
            if shutting_down() { format!("stopped, {}", stop_reason()) } else { String::from_utf8_lossy(&res.stderr).trim().to_string() }
        });
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
pub(crate) fn defer_deadline(requested: DateTime<Utc>, first: bool, cron: &str, cfg: &JobCfg) -> DateTime<Utc> {
    let max = if first { cfg.max_defer_first } else { cfg.max_defer };
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
fn job_command(prog: &std::path::Path, app: &str, cfg: &JobCfg) -> Command {
    let mut cmd = Command::new(prog);
    cmd.env("PGAPPNAME", app);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let nice = cfg.nice.clamp(0, 19);
        let ioprio = parse_ionice(cfg.ionice.as_deref().unwrap_or("best-effort-7")).unwrap_or_else(|e| {
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
fn prune(c: &Ctx, cfg: &JobCfg, path: &str, max_backups: i32, max_days: i32) -> Result<Vec<String>, String> {
    let b = cfg.bucket()?;
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
fn restore_key_into(c: &Ctx, cfg: &JobCfg, admin: &mut Client, b: &Bucket, key: &str, into: &str, app: &str) -> Result<i64, String> {
    // template0: the dump brings its own CREATE EXTENSION pgbx, so start from a database without it
    admin
        .batch_execute(&format!("CREATE DATABASE \"{into}\" TEMPLATE template0"))
        .map_err(|e| format!("create {into}: {}", pe(e)))?;
    // the target is a NEW database: losing the tail of it in a crash just means restoring again
    let sync = if cfg.restore_sync { "on" } else { "off" };
    let mut child = job_command(&client_tool(c, "pg_restore").0, app, cfg)
        .env("PGOPTIONS", format!("-c synchronous_commit={sync}"))
        .args(["-h", &c.socket, "-p", &c.port.to_string(), "-U", "postgres", "--no-owner", "-d", into])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn pg_restore: {e}"))?;
    set_child(child.id());
    let mut stdin = child.stdin.take().unwrap();
    let dl = transfer::download_resumable(b, key, &mut stdin, cfg.download_kbps);
    drop(stdin); // EOF for pg_restore
    if dl.is_err() && shutting_down() {
        let _ = child.kill();
    }
    let out = child.wait_with_output().map_err(pe);
    set_child(0);
    let out = out?;
    if shutting_down() {
        if stop_reason() == "cancelled" {
            // cancelled: the half-restored NEW database (created above) is of no use to anyone
            let _ = admin.batch_execute(&format!("DROP DATABASE IF EXISTS \"{into}\" WITH (FORCE)"));
        }
        return Err(format!("restore stopped, {}", stop_reason()));
    }
    if !out.status.success() {
        return Err(format!("pg_restore failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let n = dl?;
    Ok(n as i64)
}

/// Newest dump at or before `at` -> a NEW database. The live one is never touched.
fn restore(c: &Ctx, cfg: &JobCfg, admin: &mut Client, path: &str, params: &str) -> Result<Done, String> {
    let p = parse_flat_json(params);
    let into = p.get("into_db").ok_or("restore needs into_db")?.clone();
    if !into.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
        return Err(format!("into_db '{into}' must be [A-Za-z0-9_]"));
    }
    let at = p
        .get("at")
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc)))
        .unwrap_or_else(Utc::now);
    let b = cfg.bucket()?;
    let key = pick_backup(c, &b, path, at)?;
    let bytes = restore_key_into(c, cfg, admin, &b, &key, &into, "pgbx_restore")?;
    // the copy must not back up into the original's folder: give it its own path (= its own name)
    let mut copy = connect(c, &into)?;
    copy.batch_execute("UPDATE pgbx.config SET path = NULL").map_err(pe)?;
    Ok(Done { key: Some(key), bytes, extra: "{}".into() })
}

/// Restore test: newest backup -> scratch database -> same number of user tables as at backup time -> drop.
fn verify(c: &Ctx, cfg: &JobCfg, admin: &mut Client, source: &mut Client, path: &str, scratch: &str) -> Result<Done, String> {
    let b = cfg.bucket()?;
    let key = pick_backup(c, &b, path, Utc::now())?;
    let result = (|| {
        let bytes = restore_key_into(c, cfg, admin, &b, &key, scratch, "pgbx_verify")?;
        let mut sc = connect(c, scratch)?;
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

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn overrun_skip_and_guard() {
        let (skip, catch) = (Overrun::Skip, Overrun::CatchUp);
        let hourly = "0 * * * *";
        // never backed up: due at once
        assert_eq!(due_backup(hourly, t("2026-10-02T02:30:00Z"), None, None, None, skip, 1.5), Ok(Due::Yes { skipped: 0, guard: false }));
        // normal: the 01:00 dump took 10 min; at 01:30 the 02:00 slot has not come yet, at 02:00 it is due
        let run = Some((t("2026-10-02T01:00:00Z"), t("2026-10-02T01:10:00Z")));
        let auto = Some(t("2026-10-02T01:00:00Z"));
        assert_eq!(due_backup(hourly, t("2026-10-02T01:30:00Z"), auto, run, run.map(|r| r.1), skip, 1.5), Ok(Due::No));
        assert_eq!(due_backup(hourly, t("2026-10-02T02:00:05Z"), auto, run, run.map(|r| r.1), skip, 1.5), Ok(Due::Yes { skipped: 0, guard: false }));
        // hourly + 70-min dump (01:00 -> 02:10): the 02:00 slot is skipped, next run 03:00, not back to back
        let run = Some((t("2026-10-02T01:00:00Z"), t("2026-10-02T02:10:00Z")));
        let good = run.map(|r| r.1);
        assert_eq!(due_backup(hourly, t("2026-10-02T02:10:05Z"), auto, run, good, skip, 1.5), Ok(Due::No));
        assert_eq!(due_backup(hourly, t("2026-10-02T03:00:01Z"), auto, run, good, skip, 1.5), Ok(Due::Yes { skipped: 1, guard: false }));
        // catch_up: right away (today's behaviour)
        assert_eq!(due_backup(hourly, t("2026-10-02T02:10:05Z"), auto, run, good, catch, 1.5), Ok(Due::Yes { skipped: 0, guard: false }));
        // the overrunning dump FAILED and the last good backup is old: waiting for 03:00 would leave > 1.5 h -> now
        let old_good = Some(t("2026-10-01T23:10:00Z"));
        assert_eq!(due_backup(hourly, t("2026-10-02T02:10:05Z"), auto, run, old_good, skip, 1.5), Ok(Due::Yes { skipped: 1, guard: true }));
        assert_eq!(due_backup(hourly, t("2026-10-02T02:10:05Z"), auto, run, None, skip, 1.5), Ok(Due::Yes { skipped: 1, guard: true }));
        // daily 02:00; a manual dump ran 01:30-02:20 over the slot: tomorrow 02:00 is 23.7 h away < 36 h -> wait
        let daily = "0 2 * * *";
        let (auto, run) = (Some(t("2026-10-01T02:00:00Z")), Some((t("2026-10-02T01:30:00Z"), t("2026-10-02T02:20:00Z"))));
        assert_eq!(due_backup(daily, t("2026-10-02T02:20:05Z"), auto, run, run.map(|r| r.1), skip, 1.5), Ok(Due::No));
        // ... but if that dump failed and yesterday's 02:05 is the last good one: 48 h > 36 h -> run now
        assert_eq!(due_backup(daily, t("2026-10-02T02:20:05Z"), auto, run, Some(t("2026-10-01T02:05:00Z")), skip, 1.5),
                   Ok(Due::Yes { skipped: 1, guard: true }));
        // several slots inside one dump are counted
        let run = Some((t("2026-10-02T01:00:00Z"), t("2026-10-02T04:30:00Z")));
        assert_eq!(due_backup(hourly, t("2026-10-02T05:00:01Z"), Some(t("2026-10-02T01:00:00Z")), run, run.map(|r| r.1), skip, 1.5),
                   Ok(Due::Yes { skipped: 3, guard: false }));
    }

    #[test]
    fn pick_priority() {
        assert!(priority("restore", "manual", "{}") < priority("backup", "manual", "{}"));
        assert!(priority("backup", "manual", "{}") < priority("backup", "schedule", "{}"));
        assert_eq!(priority("backup", "schedule", "{\"manual\": true, \"coalesced\": 1}"), priority("backup", "manual", "{}"));
        assert_eq!(priority("backup", "first", "{}"), priority("backup", "schedule", "{}"));
        assert!(priority("backup", "first", "{}") < priority("verify", "manual", "{}"));
        assert!(priority("verify", "schedule", "{}") < priority("prune", "manual", "{}"));
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
