//! Point-in-time restore (optional, `pgbx.pitr = on`): the worker side.
//!
//! The data path lives in the pgbx CLI (`pgbx wal-push` as archive_command, `pgbx pitr backup` for base
//! backups, `pgbx wal-get` during recovery). Every poll the worker's main thread (never blocking on S3):
//! 1. writes `<work_dir>/pgbx-wal.conf` (0600; S3 settings + the PATH of the credentials file, never keys),
//! 2. turns WAL dropped by wal-push under `pgbx.wal_queue_max` (`<work_dir>/wal-drops.log`) into a 'wal_gap'
//!    history row + alert, and closes it once a base backup that STARTED after the last drop has finished,
//! 3. watches archiving (pg_stat_archiver + the .ready backlog) as a 'wal_archive' incident: one alert when it
//!    starts, a reminder every hour, one 'recovered' alert only when WAL really reached S3 again and nothing was
//!    dropped recently (a drop also tells Postgres "archived", so the archiver alone looks healthy),
//! 4. queues base backups on `pgbx.pitr_schedule` (and a healing one after a gap, with backoff).
//!
//! A base backup is an ordinary job of the server-wide queue (ADR 0001): kind 'base_backup' in the admin database,
//! started on a job thread with a slot, which runs `pgbx pitr backup` as its child (cancel / shutdown kill it).
//! History rows ('base_backup', 'wal_gap', 'wal_archive') live in the admin database. Alerts and notifications for
//! whole-server incidents are sent from a thread of their own.
//! Designs ported from pgBackRest (MIT, see NOTICE): queue-max drops, async push, prefetch, time-based expiry.

use crate::extras::incident_async;
use crate::worker::{log, pe, set_child, setting, shutting_down, stop_reason, Ctx, Done, JobCfg};
use crate::*;
use postgres::Client;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

pub(crate) const CONF_NAME: &str = "pgbx-wal.conf";

// ------------------------------------------------------------------------------------------- parsing (pure)

/// Size setting -> bytes. '4GB', '512 MB', '1.5GB', '1048576' (bytes). '0', 'off', 'none', 'unlimited' -> None.
pub(crate) fn parse_size(raw: &str) -> Result<Option<u64>, String> {
    let t = raw.trim().to_ascii_lowercase().replace(' ', "");
    if matches!(t.as_str(), "" | "0" | "off" | "none" | "unlimited" | "-1") {
        return Ok(None);
    }
    let split = t.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(t.len());
    let n: f64 = t[..split].parse().map_err(|_| format!("'{raw}' is not a size like '4GB', '512MB' or 'off'"))?;
    let mult: u64 = match &t[split..] {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1 << 40,
        _ => return Err(format!("'{raw}' is not a size like '4GB', '512MB' or 'off'")),
    };
    let b = (n * mult as f64) as u64;
    Ok(if b == 0 { None } else { Some(b) })
}

/// Duration setting -> seconds. '15 min', '1h', '2 hours', '90s', '7 days'. A bare number is minutes.
pub(crate) fn parse_duration(raw: &str) -> Result<i64, String> {
    let t = raw.trim().to_ascii_lowercase();
    let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let n: i64 = t[..split].parse().map_err(|_| format!("'{raw}' is not a duration like '15 min' or '7 days'"))?;
    let unit = match t[split..].trim() {
        "" | "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "h" | "hr" | "hour" | "hours" => 3600,
        "d" | "day" | "days" => 86_400,
        _ => return Err(format!("'{raw}' is not a duration like '15 min' or '7 days'")),
    };
    Ok(n * unit)
}

pub(crate) fn human_bytes(b: u64) -> String {
    let units = ["B", "kB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else { format!("{v:.1} {}", units[i]) }
}

pub(crate) fn human_secs(s: i64) -> String {
    match s {
        s if s < 120 => format!("{s}s"),
        s if s < 7200 => format!("{}m", s / 60),
        s if s < 172_800 => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
        s => format!("{}d{}h", s / 86_400, (s % 86_400) / 3600),
    }
}

/// `<epoch> <file> ...` lines written by `pgbx wal-push` for every dropped file.
pub(crate) fn parse_drops_log(text: &str) -> Vec<(i64, String)> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let t = it.next()?.parse().ok()?;
            let f = it.next()?;
            (f.len() >= 24 && f[..24].chars().all(|c| c.is_ascii_hexdigit()) || f.ends_with(".history")).then(|| (t, f.to_string()))
        })
        .collect()
}

/// Drops newer than both the newest already recorded in a gap and the high-water mark (WAL names sort in order).
pub(crate) fn new_drops<'a>(drops: &'a [(i64, String)], known: Option<&str>, hwm: Option<&str>) -> Vec<&'a (i64, String)> {
    let floor = match (known, hwm) {
        (Some(a), Some(b)) => Some(if a > b { a } else { b }),
        (a, b) => a.or(b),
    };
    drops.iter().filter(|(_, w)| floor.is_none_or(|f| w.as_str() > f)).collect()
}

/// Class of a wal-push error — a word, never the raw text (it may quote URLs or paths).
pub(crate) fn error_class(text: &str) -> &'static str {
    let l = text.to_ascii_lowercase();
    let has = |ws: &[&str]| ws.iter().any(|w| l.contains(w));
    if has(&["different checksum"]) {
        "checksum_conflict"
    } else if has(&["403", "accessdenied", "signaturedoesnotmatch", "invalidaccesskeyid", "forbidden"]) {
        "s3_auth"
    } else if has(&["nosuchbucket", "404"]) {
        "s3_bucket"
    } else if has(&["timeout", "timed out"]) {
        "s3_timeout"
    } else if has(&["connection refused", "dns", "error sending request", "could not resolve", "unreachable", "connect"]) {
        "s3_unreachable"
    } else if has(&["credentials"]) {
        "credentials"
    } else if has(&["no space left"]) {
        "disk_full"
    } else if has(&["permission denied"]) {
        "permissions"
    } else {
        "other"
    }
}

/// One look at WAL archiving.
#[derive(Debug, Clone, Default)]
pub(crate) struct WalObs {
    pub failing: bool,
    pub failed_count: i64,
    pub ready_count: i64,
    pub oldest_ready_secs: i64,
    pub ready_bytes: u64,
}

/// Why archiving is a problem right now (None = fine).
pub(crate) fn judge(o: &WalObs, after: i64, size: Option<u64>) -> Option<String> {
    let mut why = vec![];
    if o.ready_count > 0 && o.oldest_ready_secs > after {
        why.push(format!(
            "WAL archiving {} for {}: {} segment(s) waiting ({} failures so far)",
            if o.failing { "failing" } else { "stuck or too slow" },
            human_secs(o.oldest_ready_secs),
            o.ready_count,
            o.failed_count
        ));
    }
    if let Some(s) = size.filter(|s| o.ready_bytes > *s) {
        why.push(format!("{} of WAL waiting to be archived (alert above {})", human_bytes(o.ready_bytes), human_bytes(s)));
    }
    (!why.is_empty()).then(|| why.join("; "))
}

/// Start of a gap = last moment known safe in S3: the earliest of the archiver's last success seen BEFORE the
/// first drop and the start of the open archiving incident; neither known: detection time.
pub(crate) fn gap_start(detected: i64, last_archived_before_drop: Option<i64>, incident_since: Option<i64>) -> i64 {
    [last_archived_before_drop, incident_since].into_iter().flatten().min().unwrap_or(detected).min(detected)
}

pub(crate) fn recover_allowed(archived_since: bool, recent_gap: bool, recent_drops: bool) -> bool {
    archived_since && !recent_gap && !recent_drops
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Incident {
    pub since: i64,
    pub last_alert: i64,
}

#[derive(Debug, PartialEq)]
pub(crate) enum Action {
    Quiet,
    Open(String),
    Remind(String),
    Recovered(i64),
}

/// Alert de-duplication: alert on start, remind every `remind` s, one 'recovered' when it really ends.
pub(crate) fn step(cur: Option<Incident>, now: i64, problem: Option<&str>, remind: i64, can_recover: bool) -> (Option<Incident>, Action) {
    match (cur, problem) {
        (None, None) => (None, Action::Quiet),
        (None, Some(p)) => (Some(Incident { since: now, last_alert: now }), Action::Open(p.into())),
        (Some(i), None) if !can_recover => (Some(i), Action::Quiet),
        (Some(i), None) => (None, Action::Recovered(now - i.since)),
        (Some(i), Some(p)) if now - i.last_alert >= remind => (Some(Incident { last_alert: now, ..i }), Action::Remind(p.into())),
        (Some(i), Some(_)) => (Some(i), Action::Quiet),
    }
}

/// Wait before the next healing base backup after `failures` failed ones: 10m, 20m, 40m, ... at most 6h.
pub(crate) fn backoff_secs(failures: u32) -> i64 {
    if failures == 0 {
        return 0;
    }
    (600i64 << (failures - 1).min(10)).min(36 * 600)
}

// ------------------------------------------------------------------------------------------- settings

pub(crate) fn enabled() -> bool {
    PITR.get()
}

/// pgbx.work_dir, or <data_directory>/../pgbx (where `pgbx wal-push` looks by default: archive_command runs in
/// the data directory).
pub(crate) fn work_dir(admin: &mut Client) -> PathBuf {
    if let Some(w) = setting(&WORK_DIR) {
        return PathBuf::from(w);
    }
    let dd: String = admin.query_one("SHOW data_directory", &[]).map(|r| r.get(0)).unwrap_or("/var/lib/postgresql/data".into());
    Path::new(&dd).parent().map(|p| p.join("pgbx")).unwrap_or(PathBuf::from("/var/lib/postgresql/pgbx"))
}

pub(crate) fn cli_path() -> PathBuf {
    if let Some(p) = setting(&CLI_PATH) {
        return PathBuf::from(p);
    }
    for p in ["/usr/local/bin/pgbx", "/usr/bin/pgbx"] {
        if Path::new(p).is_file() {
            return PathBuf::from(p);
        }
    }
    PathBuf::from("pgbx")
}

fn margin_secs() -> i64 {
    setting(&WAL_GAP_MARGIN).and_then(|v| parse_duration(&v).ok()).unwrap_or(60).max(0)
}

fn retention_days() -> i64 {
    setting(&PITR_RETENTION).and_then(|v| parse_duration(&v).ok()).map(|s| (s / 86_400).max(1)).unwrap_or(7)
}

// ------------------------------------------------------------------------------------------- conf file

fn render_conf(c: &Ctx, wd: &Path, sid: &str, seg: i64, pgdata: &str) -> String {
    let mut s = String::from("# written by the pgbx worker (pgbx.pitr = on); S3 keys are never stored here, only the path of the credentials file\n");
    let mut kv = |k: &str, v: String| {
        if !v.is_empty() {
            s.push_str(&format!("{k}={v}\n"));
        }
    };
    kv("s3_endpoint", setting(&S3_ENDPOINT).unwrap_or_default());
    kv("s3_bucket", setting(&S3_BUCKET).unwrap_or_default());
    kv("s3_region", setting(&S3_REGION).unwrap_or_default());
    kv("credentials_file", setting(&CREDENTIALS_FILE).unwrap_or_default());
    kv("server_name", c.server.clone());
    kv("system_id", sid.to_string());
    kv("work_dir", wd.display().to_string());
    kv("pgdata", pgdata.to_string());
    kv("wal_queue_max", parse_size(&setting(&WAL_QUEUE_MAX).unwrap_or("4GB".into())).ok().flatten().map(|v| v.to_string()).unwrap_or_default());
    kv("wal_segment_size", seg.to_string());
    kv("async", "on".into());
    kv("process_max", "4".into());
    kv("compress_level", "1".into());
    kv("archive_timeout", "60".into());
    kv("socket_dir", c.socket.clone());
    kv("port", c.port.to_string());
    kv("bindir", c.bindir.display().to_string());
    kv("retention_days", retention_days().to_string());
    s
}

fn write_if_changed(p: &Path, body: &str) -> bool {
    if std::fs::read_to_string(p).ok().as_deref() == Some(body) {
        return false;
    }
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = p.with_extension("tmp");
    let ok = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(body.as_bytes()))
        .and_then(|_| std::fs::rename(&tmp, p))
        .is_ok();
    if !ok {
        log(&format!("could not write {}", p.display()));
    }
    ok
}

// ------------------------------------------------------------------------------------------- children

/// `pgbx pitr publish-gaps` children (one at a time); a base backup is a job (see `run_base_backup`).
static SIDE: Mutex<Vec<Child>> = Mutex::new(Vec::new());
/// The conf file the last tick wrote, for base backup jobs (job threads never read settings).
static CONF: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Shutdown: stop a running gaps upload.
pub(crate) fn stop() {
    for mut c in SIDE.lock().unwrap_or_else(|e| e.into_inner()).drain(..) {
        let _ = c.kill();
        let _ = c.wait();
    }
}

fn reap_side() {
    SIDE.lock().unwrap_or_else(|e| e.into_inner()).retain_mut(|c| matches!(c.try_wait(), Ok(None)));
}

/// What a base backup job needs, read on the main thread when it starts.
pub(crate) fn job_conf() -> Option<(PathBuf, PathBuf)> {
    CONF.lock().unwrap_or_else(|e| e.into_inner()).clone().map(|c| (c, cli_path()))
}

// ------------------------------------------------------------------------------------------- tick

/// Every worker poll, on the main thread (admin database connection). Never fails the poll: problems are logged.
pub(crate) fn tick(c: &Ctx, admin: &mut Client) {
    reap_side();
    if !enabled() {
        return;
    }
    if let Err(e) = tick_inner(c, admin) {
        log(&format!("pitr: {e}"));
    }
}

fn tick_inner(c: &Ctx, admin: &mut Client) -> Result<(), String> {
    let wd = work_dir(admin);
    if !wd.exists() {
        std::fs::create_dir_all(&wd).map_err(|e| format!("create {}: {e}", wd.display()))?;
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&wd, std::fs::Permissions::from_mode(0o700));
    }
    let r = admin
        .query_one(
            "SELECT (SELECT system_identifier::text FROM pg_control_system()),
                    pg_size_bytes(current_setting('wal_segment_size'))::bigint, current_setting('data_directory')",
            &[],
        )
        .map_err(pe)?;
    let (sid, seg, pgdata): (String, i64, String) = (r.get(0), r.get(1), r.get(2));
    let conf = wd.join(CONF_NAME);
    write_if_changed(&conf, &render_conf(c, &wd, &sid, seg, &pgdata));
    *CONF.lock().unwrap_or_else(|e| e.into_inner()) = Some(conf);
    admin
        .execute(
            "INSERT INTO pgbx.pitr_state (id, system_id, work_dir) VALUES (1, $1, $2)
             ON CONFLICT (id) DO UPDATE SET system_id = $1, work_dir = $2",
            &[&sid, &wd.display().to_string()],
        )
        .map_err(pe)?;
    // gaps first: a drop makes Postgres believe the WAL was archived, so the incident check must already know
    if let Err(e) = gaps(c, admin, &wd) {
        log(&format!("pitr gap watch: {e}"));
    }
    if let Err(e) = incident(c, admin, &wd) {
        log(&format!("pitr archive watch: {e}"));
    }
    publish_gaps(admin, &wd);
    schedule(admin)
}

fn schedule(admin: &mut Client) -> Result<(), String> {
    let cron = schedule::to_cron(&setting(&PITR_SCHEDULE).unwrap_or("daily at 01:00".into())).unwrap_or("0 1 * * *".into());
    let r = admin
        .query_one(
            "SELECT (SELECT max(requested_at) FROM pgbx.history WHERE kind='base_backup' AND trigger IN ('schedule','first')),
                    EXISTS (SELECT 1 FROM pgbx.history WHERE kind='base_backup' AND state IN ('queued','running')),
                    current_setting('archive_mode') <> 'off'",
            &[],
        )
        .map_err(pe)?;
    let (last, busy, archiving): (Option<std::time::SystemTime>, bool, bool) = (r.get(0), r.get(1), r.get(2));
    if busy || !archiving {
        return Ok(());
    }
    let last = last.map(chrono::DateTime::<chrono::Utc>::from);
    if crate::worker::is_due(&cron, last)? {
        let trigger = if last.is_none() { "first" } else { "schedule" };
        admin.execute("INSERT INTO pgbx.history (kind, trigger) VALUES ('base_backup', $1)", &[&trigger]).map_err(pe)?;
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------- base backup job

/// The 'base_backup' job, on its job thread: `pgbx pitr backup --expire --json --conf <work_dir>/pgbx-wal.conf` as a
/// child (cancel and shutdown kill it, like pg_dump), its one JSON line becomes the history row.
pub(crate) fn run_base_backup(cfg: &JobCfg) -> Result<Done, String> {
    let (conf, cli) = cfg.pitr.clone().ok_or("point-in-time restore is not set up yet (pgbx.pitr off, or the worker has not written pgbx-wal.conf)")?;
    let child = Command::new(&cli)
        .args(["pitr", "backup", "--expire", "--json", "--conf"])
        .arg(&conf)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start {} pitr backup: {e} (set pgbx.cli_path)", cli.display()))?;
    set_child(child.id());
    let out = child.wait_with_output();
    set_child(0);
    let out = out.map_err(pe)?;
    if shutting_down() {
        return Err(format!("base backup stopped, {}", stop_reason()));
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !text.contains("\"ok\":true") {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(serde_error(&text).unwrap_or_else(|| err.lines().last().unwrap_or("base backup failed").to_string()));
    }
    Ok(Done { key: json_str(&text, "key"), bytes: json_num(&text, "bytes"), extra: text })
}

/// After a base backup job: its params from the CLI's JSON, and the kept backups into pitr_state.
pub(crate) fn record_base_backup(cl: &mut Client, id: i64, out: &str) -> Result<(), String> {
    cl.execute(
        "UPDATE pgbx.history SET params = params || jsonb_build_object('label', j->>'label', 'start_time', j->'info'->>'start_time',
                'stop_time', j->'info'->>'stop_time', 'start_lsn', j->'info'->>'start_lsn', 'stop_lsn', j->'info'->>'stop_lsn',
                'timeline', j->'info'->'timeline', 'mb_per_s', j->'mb_per_s', 'expired_backups', j->'expired_backups',
                'expired_wal', j->'expired_wal', 'expire_error', j->'expire_error')
           FROM (SELECT $2::text::jsonb AS j) x WHERE id=$1",
        &[&id, &out],
    )
    .map_err(pe)?;
    cl.execute(
        "INSERT INTO pgbx.pitr_state AS s (id, base_backups, updated_at) VALUES (1, coalesce(($1::text::jsonb)->'kept', '[]'), now())
         ON CONFLICT (id) DO UPDATE SET base_backups = coalesce(($1::text::jsonb)->'kept', s.base_backups), updated_at = now()",
        &[&out],
    )
    .map_err(pe)?;
    Ok(())
}

/// The "error" string of a one-line JSON object, without a JSON library.
fn serde_error(s: &str) -> Option<String> {
    json_str(s, "error")
}

fn json_str(s: &str, k: &str) -> Option<String> {
    let i = s.find(&format!("\"{k}\":\""))? + k.len() + 4;
    let mut out = String::new();
    let mut esc = false;
    for ch in s[i..].chars() {
        if esc {
            out.push(match ch {
                'n' => '\n',
                't' => '\t',
                c => c,
            });
            esc = false;
        } else if ch == '\\' {
            esc = true;
        } else if ch == '"' {
            return Some(out);
        } else {
            out.push(ch);
        }
    }
    None
}

fn json_num(s: &str, k: &str) -> i64 {
    s.find(&format!("\"{k}\":"))
        .map(|i| s[i + k.len() + 3..].chars().take_while(|c| c.is_ascii_digit()).collect::<String>())
        .and_then(|d| d.parse().ok())
        .unwrap_or(0)
}

// ------------------------------------------------------------------------------------------- gaps

const HWM_FILE: &str = "wal-drops.hwm";

fn read_hwm(wd: &Path) -> Option<String> {
    std::fs::read_to_string(wd.join(HWM_FILE)).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn write_hwm(wd: &Path, wal: &str) {
    if read_hwm(wd).is_none_or(|h| wal > h.as_str()) {
        let _ = std::fs::write(wd.join(HWM_FILE), format!("{wal}\n"));
    }
}

fn drops(wd: &Path) -> Vec<(i64, String)> {
    let mut v = parse_drops_log(&std::fs::read_to_string(wd.join(crate::pitr::DROPS_LOG)).unwrap_or_default());
    v.sort_by(|a, b| a.1.cmp(&b.1));
    v.dedup_by(|a, b| a.1 == b.1);
    v
}

pub(crate) const DROPS_LOG: &str = "wal-drops.log";

fn gaps(c: &Ctx, admin: &mut Client, wd: &Path) -> Result<(), String> {
    let all = drops(wd);
    let known: Option<String> = admin
        .query_one("SELECT max(params->>'last_wal') FROM pgbx.history WHERE kind='wal_gap'", &[])
        .map_err(pe)?
        .get(0);
    let hwm = read_hwm(wd);
    let new = new_drops(&all, known.as_deref(), hwm.as_deref());
    // remember the archiver's last success while nothing new is dropped: once drops start, pg_stat_archiver
    // advances for WAL that never reached S3, so the value from before the first drop is the one that counts
    let safe_file = wd.join("wal-last-archived");
    if new.is_empty() {
        let la: Option<i64> = admin
            .query_one("SELECT extract(epoch FROM last_archived_time)::bigint FROM pg_stat_archiver", &[])
            .map_err(pe)?
            .get(0);
        if let Some(la) = la {
            let _ = std::fs::write(&safe_file, format!("{la}\n"));
        }
    }
    let open = admin
        .query_opt("SELECT id FROM pgbx.history WHERE kind='wal_gap' AND state='running' ORDER BY id DESC LIMIT 1", &[])
        .map_err(pe)?;
    if let (Some(first), Some(last)) = (new.first(), new.last()) {
        let n = new.len() as i64;
        let last_drop_epoch = new.iter().map(|d| d.0).max().unwrap_or(0);
        write_hwm(wd, &last.1);
        match &open {
            Some(r) => {
                let id: i64 = r.get(0);
                admin
                    .execute(
                        "UPDATE pgbx.history SET params = params || jsonb_build_object('last_wal', $2::text,
                                'last_seen', to_timestamp($4::bigint), 'dropped', coalesce((params->>'dropped')::bigint, 0) + $3) WHERE id=$1",
                        &[&id, &last.1, &n, &last_drop_epoch],
                    )
                    .map_err(pe)?;
            }
            None => {
                let before: Option<i64> = std::fs::read_to_string(&safe_file).ok().and_then(|t| t.trim().parse().ok());
                let r = admin
                    .query_one(
                        "SELECT extract(epoch FROM now())::bigint,
                                (SELECT extract(epoch FROM started)::bigint FROM pgbx.history
                                  WHERE kind='wal_archive' AND state='running' ORDER BY id DESC LIMIT 1)",
                        &[],
                    )
                    .map_err(pe)?;
                let safe = gap_start(r.get(0), before, r.get(1)).min(first.0);
                let margin = margin_secs();
                let at: String = admin
                    .query_one("SELECT date_trunc('second', to_timestamp(($1::bigint - $2::bigint)::double precision))::text", &[&safe, &margin])
                    .map_err(pe)?
                    .get(0);
                let msg = format!(
                    "pgbx wal-push DROPPED {n} WAL file(s) from {} because the archive queue exceeded pgbx.wal_queue_max. \
                     Postgres keeps running, but point-in-time restore is impossible from {at} (last moment known safe in S3 \
                     minus pgbx.wal_gap_margin) until the next base backup starts (queued automatically once WAL reaches S3 again)",
                    first.1
                );
                let id: i64 = admin
                    .query_one(
                        "INSERT INTO pgbx.history (kind, trigger, state, started, params, error)
                         VALUES ('wal_gap', 'schedule', 'running', to_timestamp($5::bigint),
                                 jsonb_build_object('first_wal', $1::text, 'last_wal', $2::text, 'last_seen', to_timestamp($7::bigint),
                                                    'dropped', $3::bigint, 'safe_until', to_timestamp($5::bigint),
                                                    'detected_at', now(), 'margin_s', $6::bigint), $4)
                         RETURNING id",
                        &[&first.1, &last.1, &n, &msg, &safe, &margin, &last_drop_epoch],
                    )
                    .map_err(pe)?
                    .get(0);
                log(&format!("WAL gap #{id}: {msg}"));
                incident_async(JobCfg::now(), c.server.clone(), "wal_gap", id, msg.clone(), true);
            }
        }
        return Ok(());
    }
    let Some(r) = open else { return Ok(()) };
    let id: i64 = r.get(0);
    // closed by a base backup that STARTED after the last drop and finished; the gap ends where it started
    let healed: Option<String> = admin
        .query_one(
            "SELECT min(coalesce((b.params->>'start_time')::timestamptz, b.started))::text FROM pgbx.history b, pgbx.history g
              WHERE g.id=$1 AND b.kind='base_backup' AND b.state='done'
                AND coalesce((b.params->>'start_time')::timestamptz, b.started) > coalesce((g.params->>'last_seen')::timestamptz, g.started)",
            &[&id],
        )
        .map_err(pe)?
        .get(0);
    if let Some(h) = healed {
        admin
            .execute(
                "UPDATE pgbx.history SET state='done', finished=now(),
                        params = params || jsonb_build_object('healed_at', $2::text::timestamptz) WHERE id=$1",
                &[&id, &h],
            )
            .map_err(pe)?;
        log(&format!("WAL gap #{id} closed: a base backup that started after it finished"));
        return Ok(());
    }
    // queue the healing base backup only when WAL really reaches S3 again (archived after the last drop), no
    // archiving incident is open, none is queued/running, and the backoff after failed healing ones has passed
    let r = admin
        .query_one(
            "SELECT coalesce((SELECT last_archived_time FROM pg_stat_archiver)
                             > (SELECT coalesce((params->>'last_seen')::timestamptz, started) FROM pgbx.history WHERE id=$1), false),
                    EXISTS (SELECT 1 FROM pgbx.history WHERE kind='wal_archive' AND state='running'),
                    EXISTS (SELECT 1 FROM pgbx.history WHERE kind='base_backup' AND state IN ('queued','running')),
                    (SELECT count(*) FROM pgbx.history WHERE kind='base_backup' AND trigger='wal_gap'
                        AND params->>'gap' = $1::text AND state='failed'),
                    coalesce((SELECT extract(epoch FROM now() - max(finished))::bigint FROM pgbx.history
                        WHERE kind='base_backup' AND trigger='wal_gap' AND params->>'gap' = $1::text AND state='failed'), 0)",
            &[&id],
        )
        .map_err(pe)?;
    let (archived_since, incident, busy, failures, since_fail): (bool, bool, bool, i64, i64) = (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4));
    let recent_drop = all.iter().map(|d| d.0).max().is_some_and(|t| chrono::Utc::now().timestamp() - t < 30);
    if archived_since && !incident && !busy && !recent_drop && (failures == 0 || since_fail >= backoff_secs(failures as u32)) {
        let job: i64 = admin
            .query_one(
                "INSERT INTO pgbx.history (kind, trigger, params) VALUES ('base_backup', 'wal_gap', jsonb_build_object('gap', $1::bigint)) RETURNING id",
                &[&id],
            )
            .map_err(pe)?
            .get(0);
        log(&format!("WAL gap #{id}: queued base backup #{job} to make point-in-time restore possible again"));
    }
    Ok(())
}

/// Write <work_dir>/gaps.json from history; when it changed, upload it with `pgbx pitr publish-gaps` (a child; a
/// restore with Postgres down reads it from S3 to refuse moments inside a gap).
fn publish_gaps(admin: &mut Client, wd: &Path) {
    let Ok(r) = admin.query_one(
        "SELECT coalesce(jsonb_agg(jsonb_build_object('id', id, 'safe_until', params->>'safe_until', 'margin_s', (params->>'margin_s')::bigint,
                    'healed_at', params->>'healed_at', 'first_wal', params->>'first_wal', 'last_wal', params->>'last_wal') ORDER BY id), '[]')::text
           FROM pgbx.history WHERE kind='wal_gap'",
        &[],
    ) else {
        return;
    };
    let body: String = r.get(0);
    // a changed file stays "pending" until an upload succeeded (marker = copy of what was published)
    let p = wd.join("gaps.json");
    write_if_changed(&p, &body);
    let pub_marker = wd.join("gaps.published");
    if std::fs::read_to_string(&pub_marker).ok().as_deref() == Some(body.as_str()) {
        return;
    }
    let mut side = SIDE.lock().unwrap_or_else(|e| e.into_inner());
    if !side.is_empty() {
        return; // one at a time
    }
    let script = format!(
        "{} pitr publish-gaps --json --conf {} && cp {} {}",
        sh(&cli_path().display().to_string()),
        sh(&wd.join(CONF_NAME).display().to_string()),
        sh(&p.display().to_string()),
        sh(&pub_marker.display().to_string())
    );
    if let Ok(ch) = Command::new("sh").arg("-c").arg(script).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        side.push(ch);
    }
}

fn sh(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ------------------------------------------------------------------------------------------- archiving incident

const REMIND_SECS: i64 = 3600;

fn observe(admin: &mut Client) -> Result<WalObs, String> {
    let r = admin
        .query_one(
            "SELECT coalesce(a.last_failed_time > coalesce(a.last_archived_time, '-infinity'), false),
                    a.failed_count,
                    (SELECT count(*) FROM pg_ls_archive_statusdir() s WHERE s.name LIKE '%.ready'),
                    coalesce((SELECT extract(epoch FROM now() - min(s.modification))::bigint
                                FROM pg_ls_archive_statusdir() s WHERE s.name LIKE '%.ready'), 0),
                    pg_size_bytes(current_setting('wal_segment_size'))::bigint
               FROM pg_stat_archiver a",
            &[],
        )
        .map_err(pe)?;
    Ok(WalObs {
        failing: r.get(0),
        failed_count: r.get(1),
        ready_count: r.get(2),
        oldest_ready_secs: r.get(3),
        ready_bytes: (r.get::<_, i64>(2).max(0) as u64) * (r.get::<_, i64>(4).max(0) as u64),
    })
}

/// Newest wal-push error in the spool (its class only).
fn last_push_error(wd: &Path) -> Option<&'static str> {
    let rd = std::fs::read_dir(wd.join("spool/push")).ok()?;
    let newest = rd
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".error"))
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())?;
    Some(error_class(&std::fs::read_to_string(newest.path()).unwrap_or_default()))
}

fn incident(c: &Ctx, admin: &mut Client, wd: &Path) -> Result<(), String> {
    let o = observe(admin)?;
    let after = parse_duration(&setting(&WAL_ALERT_AFTER).unwrap_or("15 min".into())).unwrap_or(900);
    let size = parse_size(&setting(&WAL_ALERT_SIZE).unwrap_or("2GB".into())).unwrap_or(Some(2 << 30));
    let class = if o.failing { last_push_error(wd) } else { None };
    let problem = judge(&o, after, size).map(|p| match class {
        Some(k) => format!("{p}; last wal-push error class: {k}"),
        None => p,
    });
    let open = admin
        .query_opt(
            "SELECT id, extract(epoch FROM started)::bigint, coalesce((params->>'last_alert')::bigint, extract(epoch FROM started)::bigint)
               FROM pgbx.history WHERE kind='wal_archive' AND state='running' ORDER BY id DESC LIMIT 1",
            &[],
        )
        .map_err(pe)?;
    let cur = open.as_ref().map(|r| Incident { since: r.get(1), last_alert: r.get(2) });
    let now: i64 = admin.query_one("SELECT extract(epoch FROM now())::bigint", &[]).map_err(pe)?.get(0);
    let can_recover = match &cur {
        None => true,
        Some(i) => {
            let r = admin
                .query_one(
                    "SELECT coalesce((SELECT last_archived_time FROM pg_stat_archiver) > to_timestamp($1::bigint), false),
                            EXISTS (SELECT 1 FROM pgbx.history WHERE kind='wal_gap' AND state='running'
                                    AND coalesce((params->>'last_seen')::timestamptz, started) > now() - interval '10 minutes')",
                    &[&i.since],
                )
                .map_err(pe)?;
            let recent_drops = drops(wd).iter().any(|d| now - d.0 < 600);
            recover_allowed(r.get(0), r.get(1), recent_drops)
        }
    };
    let (_next, act) = step(cur, now, problem.as_deref(), REMIND_SECS, can_recover);
    let id: i64 = open.as_ref().map(|r| r.get(0)).unwrap_or(0);
    match act {
        Action::Quiet => {
            if let (Some(_), Some(p)) = (&open, &problem) {
                let _ = admin.execute("UPDATE pgbx.history SET error=$2 WHERE id=$1 AND error IS DISTINCT FROM $2", &[&id, p]);
            }
        }
        Action::Open(p) => {
            let id: i64 = admin
                .query_one(
                    "INSERT INTO pgbx.history (kind, trigger, state, started, params, error)
                     VALUES ('wal_archive', 'schedule', 'running', now(), jsonb_build_object('last_alert', $1::bigint), $2) RETURNING id",
                    &[&now, &p],
                )
                .map_err(pe)?
                .get(0);
            log(&format!("WAL archiving incident #{id}: {p}"));
            incident_async(JobCfg::now(), c.server.clone(), "wal_archive", id, p.clone(), true);
        }
        Action::Remind(p) => {
            let _ = admin.execute(
                "UPDATE pgbx.history SET params = params || jsonb_build_object('last_alert', $2::bigint), error=$3 WHERE id=$1",
                &[&id, &now, &p],
            );
            incident_async(JobCfg::now(), c.server.clone(), "wal_archive", id, format!("still: {p}"), true);
        }
        Action::Recovered(secs) => {
            let _ = admin.execute("UPDATE pgbx.history SET state='done', finished=now() WHERE id=$1", &[&id]);
            let msg = format!("recovered: WAL archiving is healthy again after {}", human_secs(secs));
            log(&format!("WAL archiving incident #{id} {msg}"));
            incident_async(JobCfg::now(), c.server.clone(), "wal_archive_recovered", id, msg.clone(), false);
        }
    }
    Ok(())
}

#[cfg(test)]
mod t {
    use super::*;

    #[test]
    fn sizes_and_durations() {
        assert_eq!(parse_size("4GB").unwrap(), Some(4 << 30));
        assert_eq!(parse_size("512 MB").unwrap(), Some(512 << 20));
        assert_eq!(parse_size("off").unwrap(), None);
        assert!(parse_size("lots").is_err());
        assert_eq!(parse_duration("7 days").unwrap(), 7 * 86_400);
        assert_eq!(parse_duration("15 min").unwrap(), 900);
        assert_eq!(parse_duration("60s").unwrap(), 60);
        assert!(parse_duration("soon").is_err());
    }

    #[test]
    fn drops_log() {
        let log = "1700000000 000000010000000000000003 queue=1 max=1\ngarbage\n1700000005 000000010000000000000004 queue=1 max=1\n";
        let d = parse_drops_log(log);
        assert_eq!(d, vec![(1700000000, "000000010000000000000003".into()), (1700000005, "000000010000000000000004".into())]);
        assert_eq!(new_drops(&d, Some("000000010000000000000003"), None).len(), 1);
        assert_eq!(new_drops(&d, None, Some("000000010000000000000004")).len(), 0);
        assert_eq!(new_drops(&d, None, None).len(), 2);
    }

    #[test]
    fn gap_start_is_earliest_known_safe() {
        assert_eq!(gap_start(1000, Some(900), Some(950)), 900);
        assert_eq!(gap_start(1000, None, Some(950)), 950);
        assert_eq!(gap_start(1000, None, None), 1000);
        assert_eq!(gap_start(1000, Some(2000), None), 1000);
    }

    #[test]
    fn no_false_recovery() {
        let i = Some(Incident { since: 100, last_alert: 100 });
        // problem gone (drops make the archiver look healthy) but drops are recent: stay open, say nothing
        assert!(!recover_allowed(true, false, true));
        assert!(!recover_allowed(true, true, false));
        assert!(!recover_allowed(false, false, false));
        assert_eq!(step(i.clone(), 200, None, 3600, false), (i.clone(), Action::Quiet));
        assert_eq!(step(i.clone(), 200, None, 3600, true), (None, Action::Recovered(100)));
        assert!(matches!(step(None, 1, Some("x"), 3600, true).1, Action::Open(_)));
        assert!(matches!(step(i.clone(), 100 + 3600, Some("x"), 3600, true).1, Action::Remind(_)));
        assert_eq!(step(i.clone(), 200, Some("x"), 3600, true).1, Action::Quiet);
    }

    #[test]
    fn judge_and_backoff() {
        let o = WalObs { failing: true, failed_count: 3, ready_count: 5, oldest_ready_secs: 1000, ready_bytes: 5 << 24 };
        assert!(judge(&o, 900, None).unwrap().contains("failing"));
        assert!(judge(&o, 2000, None).is_none());
        assert!(judge(&o, 2000, Some(1 << 20)).unwrap().contains("waiting to be archived"));
        assert_eq!(backoff_secs(0), 0);
        assert_eq!(backoff_secs(1), 600);
        assert_eq!(backoff_secs(2), 1200);
        assert_eq!(backoff_secs(20), 21_600);
        assert_eq!(error_class("upload x: gave up: error sending request for url"), "s3_unreachable");
        assert_eq!(error_class("already archived with a DIFFERENT checksum"), "checksum_conflict");
        assert_eq!(error_class("HTTP 403"), "s3_auth");
    }

    #[test]
    fn json_error() {
        assert_eq!(serde_error(r#"{"ok":false,"error":"refusing: \"x\" bad"}"#).unwrap(), "refusing: \"x\" bad");
        assert!(serde_error(r#"{"ok":true}"#).is_none());
    }
}
