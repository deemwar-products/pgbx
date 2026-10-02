//! Point-in-time restore (optional, per server): base backups, retention, and `pgbx pitr restore`.
//!
//!   s3://<bucket>/<server>/<system_id>/
//!     base/<label>/base.tar.zst     pg_basebackup -Ft -X none (tar of the data directory), zstd, multipart, parallel
//!     base/<label>/backup.json      written LAST: start/stop LSN + time, timeline, sizes, sha256 (no json = incomplete)
//!     wal/<timeline>/<file>.zst     archived by `pgbx wal-push` (see wal.rs)
//!     gaps.json                     WAL dropped under disk pressure (written by the extension): no restore inside
//!
//! Retention (pgBackRest's time-based expire, MIT, see NOTICE): keep every base backup that is needed to restore to
//! any moment of the last `retention_days` (the newest backup that STOPPED before the cutoff included), always keep
//! the newest one, and delete WAL older than the oldest kept backup's start segment (history files are kept).

use crate::s3x;
use crate::wal::{self, Conf};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;

pub const FORMAT: &str = "pgbx-base-1";

#[derive(Clone, Debug, PartialEq)]
pub struct BaseInfo {
    pub label: String,
    pub system_id: String,
    pub timeline: u32,
    pub start_lsn: String,
    pub stop_lsn: String,
    pub start_time: DateTime<Utc>,
    pub stop_time: DateTime<Utc>,
    pub pg_version: String,
    pub segment_size: u64,
    pub bytes: u64,
    pub tar_bytes: u64,
    pub sha256: String,
}

fn ts(t: &DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

impl BaseInfo {
    pub fn to_json(&self) -> Value {
        json!({"format": FORMAT, "label": self.label, "system_id": self.system_id, "timeline": self.timeline,
               "start_lsn": self.start_lsn, "stop_lsn": self.stop_lsn, "start_time": ts(&self.start_time),
               "stop_time": ts(&self.stop_time), "pg_version": self.pg_version, "segment_size": self.segment_size,
               "bytes": self.bytes, "tar_bytes": self.tar_bytes, "sha256": self.sha256})
    }
    pub fn from_json(v: &Value) -> Option<BaseInfo> {
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(String::from);
        let t = |k: &str| s(k).and_then(|x| DateTime::parse_from_rfc3339(&x).ok()).map(|d| d.with_timezone(&Utc));
        Some(BaseInfo {
            label: s("label")?,
            system_id: s("system_id")?,
            timeline: v.get("timeline")?.as_u64()? as u32,
            start_lsn: s("start_lsn")?,
            stop_lsn: s("stop_lsn")?,
            start_time: t("start_time")?,
            stop_time: t("stop_time")?,
            pg_version: s("pg_version").unwrap_or_default(),
            segment_size: v.get("segment_size").and_then(|x| x.as_u64()).unwrap_or(16 << 20),
            bytes: v.get("bytes").and_then(|x| x.as_u64()).unwrap_or(0),
            tar_bytes: v.get("tar_bytes").and_then(|x| x.as_u64()).unwrap_or(0),
            sha256: s("sha256")?,
        })
    }
}

/// A recorded WAL gap: no restore to a moment in [safe_until - margin, end) (end None = still open).
#[derive(Clone, Debug, PartialEq)]
pub struct Gap {
    pub id: i64,
    pub safe_until: DateTime<Utc>,
    pub margin_s: i64,
    pub end: Option<DateTime<Utc>>,
}

pub fn parse_gaps(v: &Value) -> Vec<Gap> {
    let t = |x: &Value, k: &str| x.get(k).and_then(|y| y.as_str()).and_then(|y| DateTime::parse_from_rfc3339(y).ok()).map(|d| d.with_timezone(&Utc));
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|g| {
                    Some(Gap {
                        id: g.get("id").and_then(|x| x.as_i64()).unwrap_or(0),
                        safe_until: t(g, "safe_until")?,
                        margin_s: g.get("margin_s").and_then(|x| x.as_i64()).unwrap_or(60),
                        end: t(g, "healed_at"),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

// ------------------------------------------------------------------------------------------------ choices (pure)

#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Time(DateTime<Utc>),
    Latest,
}

/// The base backup to restore from for `target`, refusing moments inside (or reached across) a WAL gap.
pub fn choose(backups: &[BaseInfo], gaps: &[Gap], target: &Target) -> Result<BaseInfo, String> {
    let mut bs: Vec<&BaseInfo> = backups.iter().collect();
    bs.sort_by_key(|b| b.stop_time);
    let pick = match target {
        Target::Latest => bs.last().copied().ok_or("no complete base backup in S3 yet")?,
        Target::Time(t) => bs.iter().rev().find(|b| b.stop_time <= *t).copied().ok_or_else(|| {
            match bs.first() {
                Some(f) => format!("no base backup finished at or before {}; the earliest restorable moment is {}", ts(t), ts(&f.stop_time)),
                None => "no complete base backup in S3 yet".to_string(),
            }
        })?,
    };
    if let Target::Time(t) = target {
        for g in gaps {
            let from = g.safe_until - chrono::Duration::seconds(g.margin_s.max(0));
            // replay from pick.start_time to t needs every WAL file in between; a gap overlapping that span breaks it
            let gap_end_ok = g.end.is_some_and(|e| e <= pick.start_time);
            if *t >= from && !gap_end_ok {
                return Err(format!(
                    "refusing: {} is inside WAL gap #{} (WAL was dropped under disk pressure from about {} until {}); \
                     pick a time before {} or after {}",
                    ts(t), g.id, ts(&from),
                    g.end.map(|e| ts(&e)).unwrap_or("now (still open)".into()),
                    ts(&from),
                    g.end.map(|e| format!("a base backup that started at or after {}", ts(&e))).unwrap_or("the next base backup".into())
                ));
            }
        }
    }
    Ok(pick.clone())
}

/// Labels of base backups to delete, and the oldest kept backup (whose start bounds WAL retention).
pub fn expire_plan(backups: &[BaseInfo], now: DateTime<Utc>, days: i64) -> (Vec<String>, Option<BaseInfo>) {
    let mut bs: Vec<&BaseInfo> = backups.iter().collect();
    bs.sort_by_key(|b| b.stop_time);
    if bs.is_empty() {
        return (vec![], None);
    }
    let cutoff = now - chrono::Duration::days(days.max(1));
    // the newest backup that stopped at or before the cutoff anchors the window; everything older goes
    let anchor = bs.iter().rposition(|b| b.stop_time <= cutoff).unwrap_or(0);
    let anchor = anchor.min(bs.len() - 1); // never delete the newest
    let gone = bs[..anchor].iter().map(|b| b.label.clone()).collect();
    (gone, Some(bs[anchor].clone()))
}

/// Archived WAL keys no longer needed once `oldest` is the oldest kept base backup. History files are kept.
pub fn wal_to_delete(keys: &[String], oldest: &BaseInfo) -> Vec<String> {
    let Some(first) = wal::segment_of_lsn(oldest.timeline, &oldest.start_lsn, oldest.segment_size) else { return vec![] };
    keys.iter()
        .filter(|k| {
            let f = k.rsplit('/').next().unwrap_or("").trim_end_matches(".zst");
            if wal::is_history(f) || !wal::archivable(f) {
                return false;
            }
            let tl_ok = f[..8] <= first[..8];
            tl_ok && f[8..24] < first[8..24]
        })
        .cloned()
        .collect()
}

/// postgresql.auto.conf lines for recovery. `copy`: a restored COPY must never archive into the original's S3
/// path or back up into its folder.
pub fn recovery_conf(restore_command: &str, target: &Target, copy: bool, server: &str, label: &str) -> String {
    let q = |s: &str| format!("'{}'", s.replace('\'', "''"));
    let mut s = format!("\n# ---- added by pgbx pitr restore ({label}) ----\nrestore_command = {}\n", q(restore_command));
    match target {
        Target::Time(t) => {
            s.push_str(&format!("recovery_target_time = {}\n", q(&t.format("%Y-%m-%d %H:%M:%S%.6f+00").to_string())));
            s.push_str("recovery_target_action = 'promote'\nrecovery_target_inclusive = on\n");
        }
        Target::Latest => {}
    }
    s.push_str("recovery_target_timeline = 'latest'\n");
    if copy {
        s.push_str(&format!(
            "# a restored copy must not archive into the original's WAL path nor back up into its folder\n\
             archive_mode = 'off'\narchive_command = ''\npgbx.pitr = 'off'\npgbx.server_name = {}\n",
            q(&format!("{server}-pitr-copy-{label}"))
        ));
    }
    s
}

pub fn restore_command(exe: &str, conf: &Path) -> String {
    format!("\"{exe}\" wal-get %f %p --conf \"{}\"", conf.display())
}

/// --time: explicit offset required, or 'latest'.
pub fn parse_target(t: &str) -> Result<Target, String> {
    if t.trim().eq_ignore_ascii_case("latest") {
        return Ok(Target::Latest);
    }
    crate::check_time(t)?;
    crate::s3restore::parse_time(t).map(Target::Time)
}

/// Is a postmaster running on this data directory? (postmaster.pid with a live pid)
pub fn running(dir: &Path) -> bool {
    let Ok(t) = std::fs::read_to_string(dir.join("postmaster.pid")) else { return false };
    let Some(pid) = t.lines().next().and_then(|l| l.trim().parse::<i32>().ok()) else { return false };
    #[cfg(unix)]
    {
        pid > 0 && unsafe { libc::kill(pid, 0) } == 0
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

#[derive(Debug)]
pub enum TargetPlan {
    Fresh,
    ReplaceStopped(PathBuf),
}

/// Restore target rules: an empty / missing directory, or (with --yes-replace-whole-server) a STOPPED data
/// directory, which is moved aside (never deleted).
pub fn plan_target(dir: &Path, replace: bool, now: DateTime<Utc>) -> Result<TargetPlan, String> {
    let empty = std::fs::read_dir(dir).map(|mut r| r.next().is_none()).unwrap_or(true);
    if empty {
        return Ok(TargetPlan::Fresh);
    }
    if !replace {
        return Err(format!(
            "refusing: {} is not empty. Restore into an empty directory (a copy you start on another port), or to \
             replace a STOPPED server's whole data directory add --yes-replace-whole-server (it is moved aside, not deleted)",
            dir.display()
        ));
    }
    if running(dir) {
        return Err(format!("refusing: Postgres is running on {}; stop it first (pg_ctl -D {} stop)", dir.display(), dir.display()));
    }
    let aside = PathBuf::from(format!("{}.pgbx-replaced-{}", dir.display(), now.format("%Y%m%dT%H%M%SZ")));
    Ok(TargetPlan::ReplaceStopped(aside))
}

// ------------------------------------------------------------------------------------------------ S3 side

pub fn backups(b: &s3::Bucket, root: &str) -> Result<Vec<BaseInfo>, String> {
    let mut out = vec![];
    for d in s3x::list_dirs(b, &format!("{root}base/"))? {
        if let Some(raw) = s3x::get(b, &format!("{d}backup.json"))? {
            if let Some(i) = serde_json::from_slice::<Value>(&raw).ok().as_ref().and_then(BaseInfo::from_json) {
                out.push(i);
            }
        }
    }
    out.sort_by_key(|b| b.stop_time);
    Ok(out)
}

pub fn gaps(b: &s3::Bucket, root: &str) -> Result<Vec<Gap>, String> {
    Ok(s3x::get(b, &format!("{root}gaps.json"))?
        .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
        .map(|v| parse_gaps(&v))
        .unwrap_or_default())
}

/// Upload <work_dir>/gaps.json (written by the extension) to the server's folder, if present.
pub fn publish_gaps(c: &Conf, b: &s3::Bucket, root: &str) -> Result<bool, String> {
    let p = c.work_dir.join("gaps.json");
    let Ok(body) = std::fs::read(&p) else { return Ok(false) };
    s3x::retry("upload gaps.json", s3x::SHORT, || s3x::put(b, &format!("{root}gaps.json"), &body))?;
    Ok(true)
}

/// Expire base backups and WAL. Returns (deleted backup labels, deleted WAL count).
pub fn expire(b: &s3::Bucket, root: &str, days: i64) -> Result<(Vec<String>, usize), String> {
    let bs = backups(b, root)?;
    let (gone, oldest) = expire_plan(&bs, Utc::now(), days);
    for l in &gone {
        for (k, _) in s3x::list(b, &format!("{root}base/{l}/"))? {
            // backup.json first: the backup stops counting before its data goes
            if k.ends_with("backup.json") {
                s3x::delete(b, &k)?;
            }
        }
        for (k, _) in s3x::list(b, &format!("{root}base/{l}/"))? {
            s3x::delete(b, &k)?;
        }
    }
    let mut n = 0;
    if let Some(o) = oldest {
        let keys: Vec<String> = s3x::list(b, &format!("{root}wal/"))?.into_iter().map(|(k, _)| k).collect();
        for k in wal_to_delete(&keys, &o) {
            s3x::delete(b, &k)?;
            n += 1;
        }
    }
    Ok((gone, n))
}

// ------------------------------------------------------------------------------------------------ base backup

/// Keeps the first bytes that pass through (backup_label is the first member of base.tar).
struct Peek<R: Read> {
    inner: R,
    head: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
}
impl<R: Read> Read for Peek<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let k = self.inner.read(buf)?;
        let mut h = self.head.lock().unwrap();
        if h.len() < 1 << 20 {
            let take = k.min((1 << 20) - h.len());
            h.extend_from_slice(&buf[..take]);
        }
        Ok(k)
    }
}

/// START WAL LOCATION and START TIMELINE from backup_label text (raw tar bytes are fine).
pub fn parse_backup_label(raw: &[u8]) -> (Option<String>, Option<u32>) {
    let t = String::from_utf8_lossy(raw);
    let lsn = t.find("START WAL LOCATION: ").and_then(|i| t[i + 20..].split_whitespace().next().map(String::from));
    let tl = t.find("START TIMELINE: ").and_then(|i| t[i + 16..].split_whitespace().next().and_then(|x| x.parse().ok()));
    (lsn, tl)
}

/// A Read that hashes and counts what passes through.
struct HashRead<R: Read> {
    inner: R,
    h: Sha256,
    n: u64,
}
impl<R: Read> Read for HashRead<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let k = self.inner.read(buf)?;
        self.h.update(&buf[..k]);
        self.n += k as u64;
        Ok(k)
    }
}

/// Pipe between threads made of owned chunks (no copies beyond the chunk itself).
pub struct ChanWriter(pub mpsc::SyncSender<Vec<u8>>);
impl Write for ChanWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.send(buf.to_vec()).map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "reader gone"))?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub struct ChanReader {
    pub rx: mpsc::Receiver<Vec<u8>>,
    pub cur: Vec<u8>,
    pub pos: usize,
}
impl Read for ChanReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.pos >= self.cur.len() {
            match self.rx.recv() {
                Ok(v) => {
                    self.cur = v;
                    self.pos = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let k = (self.cur.len() - self.pos).min(buf.len());
        buf[..k].copy_from_slice(&self.cur[self.pos..self.pos + k]);
        self.pos += k;
        Ok(k)
    }
}

/// "write-ahead log start point: 0/2000028 on timeline 1" / "write-ahead log end point: 0/2000100"
pub fn parse_basebackup_stderr(s: &str) -> (Option<String>, Option<u32>, Option<String>) {
    let mut start = None;
    let mut tl = None;
    let mut stop = None;
    for l in s.lines() {
        if let Some(i) = l.find("write-ahead log start point: ") {
            let rest = &l[i + 29..];
            let mut it = rest.split_whitespace();
            start = it.next().map(String::from);
            if let Some(j) = rest.find("on timeline ") {
                tl = rest[j + 12..].trim().trim_end_matches('.').parse().ok();
            }
        }
        if let Some(i) = l.find("write-ahead log end point: ") {
            stop = l[i + 27..].split_whitespace().next().map(String::from);
        }
    }
    (start, tl, stop)
}

fn pg_tool(c: &Conf, name: &str) -> PathBuf {
    let p = Path::new(&c.bindir).join(name);
    if !c.bindir.is_empty() && p.is_file() { p } else { PathBuf::from(name) }
}

/// Take a base backup: pg_basebackup -D - -Ft -X none (server verifies page checksums) -> sha256 -> zstd
/// (multi-threaded) -> parallel multipart upload; then backup.json; then expire.
/// pg_basebackup must never outlive this process unreaped: in a container Postgres often runs as PID 1, an orphan is
/// reparented to the postmaster, and the postmaster treats an unknown child that died by a signal (pg_basebackup
/// gets SIGPIPE once we are gone) as a crashed backend: it restarts the whole server. So when the worker stops this
/// process (cancel, shutdown: SIGTERM), stop pg_basebackup and reap it before exiting.
#[cfg(unix)]
fn reap_on_term(pid: u32) {
    use std::sync::atomic::{AtomicI32, Ordering};
    static CHILD: AtomicI32 = AtomicI32::new(0);
    extern "C" fn on_term(_: libc::c_int) {
        // async-signal-safe only: kill, waitpid, _exit
        let pid = CHILD.load(Ordering::SeqCst);
        unsafe {
            if pid > 0 {
                libc::kill(pid, libc::SIGTERM);
                libc::waitpid(pid, std::ptr::null_mut(), 0);
            }
            libc::_exit(1);
        }
    }
    CHILD.store(pid as i32, Ordering::SeqCst);
    let h = on_term as extern "C" fn(libc::c_int) as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGTERM, h);
        libc::signal(libc::SIGINT, h);
    }
}
#[cfg(not(unix))]
fn reap_on_term(_pid: u32) {}

pub fn base_backup(c: &Conf, expire_after: bool) -> Result<Value, String> {
    let mut pg = postgres::Config::new();
    pg.host(&c.socket_dir).port(c.port).user("postgres").dbname("postgres").application_name("pgbx pitr");
    let mut cl = pg.connect(postgres::NoTls).map_err(|e| format!("connect: {}", crate::pe(e)))?;
    let r = cl
        .query_one(
            "SELECT (SELECT system_identifier::text FROM pg_control_system()),
                    (SELECT count(*) FROM pg_tablespace WHERE spcname NOT IN ('pg_default','pg_global')),
                    current_setting('server_version'), pg_size_bytes(current_setting('wal_segment_size'))::bigint,
                    current_setting('archive_mode')",
            &[],
        )
        .map_err(crate::pe)?;
    let (sid, ntbs, ver, seg, amode): (String, i64, String, i64, String) = (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4));
    drop(cl);
    if ntbs > 0 {
        return Err(format!(
            "refusing: this server has {ntbs} extra tablespace(s); point-in-time base backups support only the default \
             tablespaces in this version (per-database dumps still cover them)"
        ));
    }
    if amode == "off" {
        return Err("archive_mode is off: WAL is not archived, so a base backup could not be replayed to a point in time; \
                    run pgbx setup pitr and restart Postgres".into());
    }
    let root = wal::root(&c.server, &sid);
    let b = s3x::bucket(&c.s3)?;
    let start_time = Utc::now();
    let label = start_time.format("%Y%m%dT%H%M%SZ").to_string();
    let key = format!("{root}base/{label}/base.tar.zst");
    let mut child = Command::new(pg_tool(c, "pg_basebackup"))
        .args(["-h", &c.socket_dir, "-p", &c.port.to_string(), "-U", "postgres", "-D", "-", "-Ft", "-X", "none", "-c", "fast", "-v"])
        .args(["-l", &format!("pgbx {label}")])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("start pg_basebackup: {e}"))?;
    reap_on_term(child.id());
    let stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let errt = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    // pg_basebackup stdout -> raw counter -> zstd (multi-threaded) -> channel -> hash -> parallel upload
    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(64);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).min(8) as u32;
    let level = 3;
    let head = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let head2 = head.clone();
    let comp = std::thread::spawn(move || -> Result<u64, String> {
        let mut src = Peek { inner: stdout, head: head2 };
        let mut enc = zstd::stream::write::Encoder::new(ChanWriter(tx), level).map_err(|e| e.to_string())?;
        let _ = enc.multithread(threads);
        let n = std::io::copy(&mut src, &mut enc).map_err(|e| format!("compress: {e}"))?;
        enc.finish().map_err(|e| format!("compress: {e}"))?;
        Ok(n)
    });
    let mut hr = HashRead { inner: ChanReader { rx, cur: vec![], pos: 0 }, h: Sha256::new(), n: 0 };
    let up = s3x::upload_parallel(&b, &key, &mut hr, c.process_max.max(4));
    drop(hr.inner.rx); // if the upload gave up, the compressor stops (broken pipe) and so does pg_basebackup
    if up.is_err() {
        let _ = child.kill();
    }
    let raw = comp.join().map_err(|_| "compressor panicked".to_string())?;
    let status = child.wait().map_err(|e| e.to_string())?;
    let err = errt.join().unwrap_or_default();
    if !status.success() || up.is_err() || raw.is_err() {
        let _ = s3x::delete(&b, &key);
        let why = up.err().or(raw.err()).unwrap_or_else(|| last_lines(&err, 5));
        return Err(format!("base backup failed: {why}"));
    }
    let stop_time = Utc::now();
    let (mut start, mut tl, mut stop) = parse_basebackup_stderr(&err);
    let (ls, lt) = parse_backup_label(&head.lock().unwrap());
    start = start.or(ls);
    tl = tl.or(lt);
    if stop.is_none() {
        // pg_basebackup -D - does not print the end point; the current LSN right after it finished is >= it
        let mut cl = pg.connect(postgres::NoTls).map_err(|e| format!("connect: {}", crate::pe(e)))?;
        stop = cl.query_one("SELECT pg_current_wal_lsn()::text", &[]).ok().map(|r| r.get(0));
    }
    let info = BaseInfo {
        label: label.clone(),
        system_id: sid.clone(),
        timeline: tl.ok_or("pg_basebackup did not report the timeline")?,
        start_lsn: start.ok_or("pg_basebackup did not report the start point")?,
        stop_lsn: stop.ok_or("pg_basebackup did not report the end point")?,
        start_time,
        stop_time,
        pg_version: ver,
        segment_size: seg as u64,
        bytes: hr.n,
        tar_bytes: raw.unwrap_or(0),
        sha256: hr.h.finalize().iter().map(|x| format!("{x:02x}")).collect(),
    };
    let body = serde_json::to_vec_pretty(&info.to_json()).unwrap();
    s3x::retry("upload backup.json", s3x::LONG, || s3x::put(&b, &format!("{root}base/{label}/backup.json"), &body))?;
    let secs = (stop_time - start_time).num_milliseconds().max(1) as f64 / 1000.0;
    let mut out = json!({"ok": true, "label": label, "key": key, "bytes": info.bytes, "tar_bytes": info.tar_bytes,
        "seconds": secs, "mb_per_s": (info.tar_bytes as f64 / 1048576.0 / secs * 10.0).round() / 10.0, "info": info.to_json()});
    let _ = publish_gaps(c, &b, &root);
    if expire_after {
        match expire(&b, &root, c.retention_days) {
            Ok((gone, n)) => {
                out["expired_backups"] = json!(gone);
                out["expired_wal"] = json!(n);
            }
            Err(e) => out["expire_error"] = json!(e),
        }
        out["kept"] = json!(backups(&b, &root).map(|v| v.iter().map(|x| x.to_json()).collect::<Vec<_>>()).unwrap_or_default());
    }
    Ok(out)
}

fn last_lines(s: &str, n: usize) -> String {
    let v: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    v[v.len().saturating_sub(n)..].join(" | ")
}

// ------------------------------------------------------------------------------------------------ restore

pub struct RestoreReq {
    pub conf: Conf,
    pub target: Target,
    pub dir: PathBuf,
    pub replace: bool,
}

pub fn find_system_id(b: &s3::Bucket, server: &str) -> Result<String, String> {
    let ids: Vec<String> = s3x::list_dirs(b, &format!("{server}/"))?
        .into_iter()
        .filter_map(|p| p.trim_end_matches('/').rsplit('/').next().map(String::from))
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
        .collect();
    match ids.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(format!("no point-in-time archive under s3://…/{server}/ (is --server-name right? is PITR enabled there?)")),
        many => Err(format!("{server} holds archives of several database systems ({}); pick one with --system-id", many.join(", "))),
    }
}

pub fn restore(req: RestoreReq) -> Result<Value, String> {
    let mut c = req.conf;
    let b = s3x::bucket(&c.s3)?;
    if c.system_id.is_empty() {
        c.system_id = find_system_id(&b, &c.server)?;
    }
    let root = wal::root(&c.server, &c.system_id);
    let bs = backups(&b, &root)?;
    let gs = gaps(&b, &root)?;
    let pick = choose(&bs, &gs, &req.target)?;
    let now = Utc::now();
    let plan = plan_target(&req.dir, req.replace, now)?;
    let copy = matches!(plan, TargetPlan::Fresh);
    if let TargetPlan::ReplaceStopped(aside) = &plan {
        std::fs::rename(&req.dir, aside).map_err(|e| format!("move {} aside to {}: {e}", req.dir.display(), aside.display()))?;
    }
    std::fs::create_dir_all(&req.dir).map_err(|e| format!("create {}: {e}", req.dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&req.dir, std::fs::Permissions::from_mode(0o700));
    }
    let key = format!("{root}base/{}/base.tar.zst", pick.label);
    let size = s3x::head(&b, &key)?.ok_or(format!("{key} is missing"))?.size;
    let t0 = std::time::Instant::now();
    // parallel ranged download -> sha256 -> channel -> zstd decode -> tar unpack
    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(64);
    let dir = req.dir.clone();
    let unpack = std::thread::spawn(move || -> Result<(), String> {
        let dec = zstd::stream::read::Decoder::new(ChanReader { rx, cur: vec![], pos: 0 }).map_err(|e| e.to_string())?;
        let mut ar = tar::Archive::new(dec);
        ar.set_preserve_permissions(true);
        ar.set_overwrite(true);
        ar.unpack(&dir).map_err(|e| format!("extract: {e}"))
    });
    struct HashW<W: Write> {
        w: W,
        h: Sha256,
    }
    impl<W: Write> Write for HashW<W> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let k = self.w.write(buf)?;
            self.h.update(&buf[..k]);
            Ok(k)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.w.flush()
        }
    }
    let mut hw = HashW { w: ChanWriter(tx), h: Sha256::new() };
    let dl = s3x::download_parallel(&b, &key, size, &mut hw, c.process_max.max(4));
    let got: String = hw.h.finalize().iter().map(|x| format!("{x:02x}")).collect();
    drop(hw.w);
    let un = unpack.join().map_err(|_| "extract panicked".to_string())?;
    let n = dl?;
    un?;
    if got != pick.sha256 {
        return Err(format!(
            "checksum mismatch for {key} (backup.json says {}, downloaded {got}); do NOT start {}",
            pick.sha256,
            req.dir.display()
        ));
    }
    let secs = t0.elapsed().as_secs_f64().max(0.001);
    // durable restore conf for recovery's restore_command (outlives this command; no secrets in it)
    let rdir = req.dir.join("pgbx-restore");
    std::fs::create_dir_all(&rdir).map_err(|e| e.to_string())?;
    let mut rc = c.clone();
    rc.work_dir = rdir.clone();
    rc.segment_size = pick.segment_size;
    rc.queue_max = None;
    let conf_path = rdir.join(wal::CONF_NAME);
    write_private(&conf_path, wal::render_conf(&rc).as_bytes())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?.display().to_string();
    let lines = recovery_conf(&restore_command(&exe, &conf_path), &req.target, copy, &c.server, &pick.label);
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(req.dir.join("postgresql.auto.conf"))
        .map_err(|e| format!("postgresql.auto.conf: {e}"))?;
    f.write_all(lines.as_bytes()).map_err(|e| e.to_string())?;
    std::fs::write(req.dir.join("recovery.signal"), b"").map_err(|e| format!("recovery.signal: {e}"))?;
    for stale in ["postmaster.pid", "postmaster.opts", "standby.signal"] {
        let _ = std::fs::remove_file(req.dir.join(stale));
    }
    let port = if copy { "5433" } else { "5432" };
    Ok(json!({
        "ok": true,
        "base_backup": pick.label,
        "base_backup_stop_time": ts(&pick.stop_time),
        "target": match &req.target { Target::Time(t) => ts(t), Target::Latest => "latest".into() },
        "data_directory": req.dir.display().to_string(),
        "mode": if copy { "copy (archive_mode=off, pgbx.pitr=off, own pgbx.server_name)" } else { "in place (old directory moved aside)" },
        "moved_aside": match &plan { TargetPlan::ReplaceStopped(a) => json!(a.display().to_string()), _ => Value::Null },
        "bytes": n, "seconds": (secs * 10.0).round() / 10.0,
        "mb_per_s": (pick.tar_bytes as f64 / 1048576.0 / secs * 10.0).round() / 10.0,
        "restore_conf": conf_path.display().to_string(),
        "start_command": format!("pg_ctl -D {} -o '-p {port}' -l {}/pgbx-restore/recovery.log start", req.dir.display(), req.dir.display()),
        "note": "pgbx never starts Postgres. Recovery replays WAL from S3 up to the target and then promotes; watch the log for 'recovery stopping before' / 'database system is ready'",
    }))
}

pub fn write_private(p: &Path, body: &[u8]) -> Result<(), String> {
    let tmp = p.with_extension("tmp");
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().create(true).truncate(true).write(true).mode(0o600).open(&tmp)
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        f.write_all(body).map_err(|e| e.to_string())?;
    }
    #[cfg(not(unix))]
    std::fs::write(&tmp, body).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, p).map_err(|e| format!("rename {}: {e}", p.display()))
}

#[cfg(test)]
mod t {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }
    fn bi(label: &str, start: &str, stop: &str, lsn: &str) -> BaseInfo {
        BaseInfo {
            label: label.into(), system_id: "7".into(), timeline: 1, start_lsn: lsn.into(), stop_lsn: lsn.into(),
            start_time: at(start), stop_time: at(stop), pg_version: "16".into(), segment_size: 16 << 20, bytes: 1,
            tar_bytes: 1, sha256: "x".into(),
        }
    }

    #[test]
    fn info_json_roundtrip() {
        let b = bi("a", "2026-10-01T01:00:00Z", "2026-10-01T01:05:00Z", "0/3000028");
        assert_eq!(BaseInfo::from_json(&b.to_json()).unwrap(), b);
    }

    #[test]
    fn choose_newest_before_time() {
        let bs = vec![
            bi("a", "2026-10-01T01:00:00Z", "2026-10-01T01:05:00Z", "0/3000028"),
            bi("b", "2026-10-02T01:00:00Z", "2026-10-02T01:05:00Z", "0/9000028"),
        ];
        let t = |s: &str| Target::Time(at(s));
        assert_eq!(choose(&bs, &[], &t("2026-10-01T12:00:00Z")).unwrap().label, "a");
        assert_eq!(choose(&bs, &[], &t("2026-10-02T01:05:00Z")).unwrap().label, "b");
        assert_eq!(choose(&bs, &[], &Target::Latest).unwrap().label, "b");
        let e = choose(&bs, &[], &t("2026-10-01T01:03:00Z")).unwrap_err();
        assert!(e.contains("earliest restorable moment is 2026-10-01T01:05:00"), "{e}");
        assert!(choose(&[], &[], &Target::Latest).is_err());
    }

    #[test]
    fn gaps_refuse_with_margin_and_heal_at_new_backup() {
        let bs = vec![
            bi("a", "2026-10-01T01:00:00Z", "2026-10-01T01:05:00Z", "0/3000028"),
            bi("heal", "2026-10-01T15:00:00Z", "2026-10-01T15:05:00Z", "0/F000028"),
        ];
        let g = vec![Gap { id: 4, safe_until: at("2026-10-01T10:00:00Z"), margin_s: 60, end: Some(at("2026-10-01T15:00:00Z")) }];
        let t = |s: &str| Target::Time(at(s));
        assert_eq!(choose(&bs, &g, &t("2026-10-01T09:58:00Z")).unwrap().label, "a"); // before gap - margin
        assert!(choose(&bs, &g, &t("2026-10-01T09:59:30Z")).unwrap_err().contains("gap #4")); // inside margin
        assert!(choose(&bs, &g, &t("2026-10-01T12:00:00Z")).is_err()); // inside
        assert!(choose(&bs, &g, &t("2026-10-01T15:03:00Z")).is_err()); // healing backup not finished yet -> from 'a' across the gap
        assert_eq!(choose(&bs, &g, &t("2026-10-01T16:00:00Z")).unwrap().label, "heal");
        let open = vec![Gap { id: 5, safe_until: at("2026-10-01T10:00:00Z"), margin_s: 0, end: None }];
        assert!(choose(&bs, &open, &t("2026-10-01T16:00:00Z")).unwrap_err().contains("still open"));
        assert_eq!(choose(&bs, &open, &t("2026-10-01T09:00:00Z")).unwrap().label, "a");
    }

    #[test]
    fn retention_keeps_window_and_newest() {
        let now = at("2026-10-10T12:00:00Z");
        let bs = vec![
            bi("d1", "2026-10-01T01:00:00Z", "2026-10-01T01:05:00Z", "0/1000028"),
            bi("d2", "2026-10-02T01:00:00Z", "2026-10-02T01:05:00Z", "0/2000028"),
            bi("d4", "2026-10-04T01:00:00Z", "2026-10-04T01:05:00Z", "0/4000028"),
            bi("d9", "2026-10-09T01:00:00Z", "2026-10-09T01:05:00Z", "0/9000028"),
        ];
        // cutoff 10-03 12:00: d2 is the newest that stopped before it -> anchor; d1 goes
        let (gone, oldest) = expire_plan(&bs, now, 7);
        assert_eq!(gone, vec!["d1"]);
        assert_eq!(oldest.unwrap().label, "d2");
        // everything old: only the newest survives
        let (gone, oldest) = expire_plan(&bs, at("2027-01-01T00:00:00Z"), 7);
        assert_eq!(gone, vec!["d1", "d2", "d4"]);
        assert_eq!(oldest.unwrap().label, "d9");
        // nothing old enough: keep all
        assert!(expire_plan(&bs, at("2026-10-05T00:00:00Z"), 30).0.is_empty());
        assert_eq!(expire_plan(&[], now, 7), (vec![], None));
    }

    #[test]
    fn wal_retention() {
        let o = bi("d2", "2026-10-02T01:00:00Z", "2026-10-02T01:05:00Z", "0/5000028");
        let k = |f: &str| format!("s/7/wal/{}/{f}.zst", &f[..8]);
        let keys: Vec<String> = [
            "000000010000000000000003", "000000010000000000000004", "000000010000000000000005", "000000010000000000000006",
            "000000010000000000000004.00000028.backup", "00000002.history", "000000020000000000000004",
        ]
        .iter()
        .map(|f| k(f))
        .collect();
        let del = wal_to_delete(&keys, &o);
        assert_eq!(del, vec![k("000000010000000000000003"), k("000000010000000000000004"), k("000000010000000000000004.00000028.backup")]);
    }

    #[test]
    fn recovery_settings() {
        let t = Target::Time(at("2026-10-01T10:00:00Z"));
        let s = recovery_conf("\"/usr/local/bin/pgbx\" wal-get %f %p --conf \"/r/pgbx-restore/pgbx-wal.conf\"", &t, true, "prod", "L1");
        assert!(s.contains("recovery_target_time = '2026-10-01 10:00:00.000000+00'"), "{s}");
        assert!(s.contains("recovery_target_action = 'promote'"));
        assert!(s.contains("archive_mode = 'off'"));
        assert!(s.contains("pgbx.pitr = 'off'"));
        assert!(s.contains("pgbx.server_name = 'prod-pitr-copy-L1'"));
        assert!(s.contains("restore_command = '\"/usr/local/bin/pgbx\" wal-get %f %p"));
        let inplace = recovery_conf("x", &Target::Latest, false, "prod", "L1");
        assert!(!inplace.contains("archive_mode") && !inplace.contains("recovery_target_time ="));
        assert!(recovery_conf("it's", &Target::Latest, false, "p", "l").contains("'it''s'"));
    }

    #[test]
    fn target_rules() {
        let d = std::env::temp_dir().join(format!("pgbx-pitr-t-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        assert!(matches!(plan_target(&d, false, Utc::now()), Ok(TargetPlan::Fresh)));
        std::fs::create_dir_all(&d).unwrap();
        assert!(matches!(plan_target(&d, false, Utc::now()), Ok(TargetPlan::Fresh)));
        std::fs::write(d.join("PG_VERSION"), "16").unwrap();
        assert!(plan_target(&d, false, Utc::now()).unwrap_err().contains("not empty"));
        assert!(matches!(plan_target(&d, true, Utc::now()), Ok(TargetPlan::ReplaceStopped(_))));
        // a live postmaster (this test process's pid) -> refused
        std::fs::write(d.join("postmaster.pid"), format!("{}\n/x\n", std::process::id())).unwrap();
        assert!(plan_target(&d, true, Utc::now()).unwrap_err().contains("running"));
        std::fs::remove_dir_all(&d).unwrap();
        assert!(parse_target("2026-10-01 10:00:00").is_err());
        assert_eq!(parse_target("latest").unwrap(), Target::Latest);
        assert!(matches!(parse_target("2026-10-01 10:00:00+00").unwrap(), Target::Time(_)));
    }

    #[test]
    fn backup_label() {
        let raw = b"backup_label\0\0\0START WAL LOCATION: 0/5000028 (file 000000010000000000000005)\nCHECKPOINT LOCATION: 0/5000060\nSTART TIMELINE: 1\n";
        assert_eq!(parse_backup_label(raw), (Some("0/5000028".into()), Some(1)));
    }

    #[test]
    fn basebackup_stderr() {
        let s = "pg_basebackup: initiating base backup, waiting for checkpoint to complete\n\
                 pg_basebackup: checkpoint completed\n\
                 pg_basebackup: write-ahead log start point: 0/2000028 on timeline 1\n\
                 pg_basebackup: write-ahead log end point: 0/2000100\n\
                 pg_basebackup: base backup completed\n";
        assert_eq!(parse_basebackup_stderr(s), (Some("0/2000028".into()), Some(1), Some("0/2000100".into())));
    }

    #[test]
    fn gaps_json() {
        let v: Value = serde_json::from_str(
            r#"[{"id":3,"safe_until":"2026-10-01T10:00:00+00:00","margin_s":30,"healed_at":"2026-10-01T15:00:00+00:00"},
                {"id":4,"safe_until":"2026-10-02T10:00:00+00:00"}]"#,
        )
        .unwrap();
        let g = parse_gaps(&v);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].margin_s, 30);
        assert!(g[1].end.is_none());
    }
}
