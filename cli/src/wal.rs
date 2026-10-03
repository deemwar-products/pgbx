//! WAL archiving for point-in-time restore: `pgbx wal-push` (archive_command) and `pgbx wal-get` (restore_command).
//!
//! Layout: s3://<bucket>/<server>/<system_id>/wal/<timeline>/<file>.zst  (zstd; x-amz-meta-sha256 = sha256 of the
//! uncompressed file). Segments, .history and .backup files all live under their timeline (the first 8 hex digits).
//!
//! Async archive-push and archive-get prefetch are ported from pgBackRest (MIT, see NOTICE):
//! - push: Postgres calls `wal-push X` one file at a time. In async mode the foreground only waits for a status
//!   file `<spool>/push/X.ok|X.error`; a single background process (one at a time, file lock) looks AHEAD through
//!   pg_wal/archive_status/*.ready and pushes up to `process_max` files in parallel, writing a status per file. It
//!   stops scheduling new work at the first error, so the next run rechecks the queue.
//! - queue max: when the WAL waiting to be archived (.ready count x segment size) exceeds `wal_queue_max`, files
//!   are DROPPED (reported to Postgres as archived) so pg_wal cannot fill the disk; each drop is appended to
//!   `<work_dir>/wal-drops.log` and the extension records the gap. The queue is measured on the ready list, not the
//!   not-yet-pushed list (pgBackRest's lesson: look-ahead files drop out of the latter while still in pg_wal).
//! - get: a segment found in `<spool>/get` (already verified) is moved into place at once; otherwise it is fetched,
//!   and a background process fetches the next `prefetch` segments in parallel so replay never waits on S3.
//!
//! Portions Copyright (c) 2013-2026, David Steele (pgBackRest), MIT License.

use crate::s3x;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const CONF_NAME: &str = "pgbx-wal.conf";
pub const DROPS_LOG: &str = "wal-drops.log";

// ------------------------------------------------------------------------------------------------ config

/// Everything wal-push / wal-get / pitr backup|restore need, from a small key=value file the worker writes
/// (`<work_dir>/pgbx-wal.conf`, 0600) or `pgbx pitr restore` writes for recovery. Credentials by FILE PATH only.
#[derive(Clone, Debug, Default)]
pub struct Conf {
    /// where this conf was read from (passed on to background processes)
    pub path: PathBuf,
    pub s3: s3x::S3Conf,
    pub server: String,
    pub system_id: String,
    pub work_dir: PathBuf,
    pub pgdata: String,
    pub queue_max: Option<u64>,
    pub segment_size: u64,
    pub async_push: bool,
    pub process_max: usize,
    pub compress_level: i32,
    pub prefetch: usize,
    pub archive_timeout: u64,
    // base backups (worker only)
    pub socket_dir: String,
    pub port: u16,
    pub bindir: String,
    pub retention_days: i64,
}

pub fn parse_conf(text: &str, path: &Path) -> Result<Conf, String> {
    let mut m = HashMap::new();
    for l in text.lines() {
        let l = l.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = l.split_once('=') {
            m.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    let need = |k: &str| m.get(k).filter(|v| !v.is_empty()).cloned().ok_or(format!("{}: {k}= is missing", path.display()));
    let num = |k: &str, d: u64| m.get(k).and_then(|v| v.parse::<u64>().ok()).unwrap_or(d);
    let work_dir = m.get("work_dir").filter(|v| !v.is_empty()).map(PathBuf::from)
        .unwrap_or_else(|| path.parent().map(|p| p.to_path_buf()).unwrap_or_default());
    Ok(Conf {
        path: path.to_path_buf(),
        s3: s3x::S3Conf {
            endpoint: need("s3_endpoint")?,
            bucket: need("s3_bucket")?,
            region: m.get("s3_region").cloned().unwrap_or_default(),
            credentials_file: need("credentials_file")?,
        },
        server: need("server_name")?,
        system_id: m.get("system_id").cloned().unwrap_or_default(),
        work_dir,
        pgdata: m.get("pgdata").cloned().unwrap_or_default(),
        queue_max: m.get("wal_queue_max").and_then(|v| v.parse::<u64>().ok()).filter(|v| *v > 0),
        segment_size: num("wal_segment_size", 16 << 20),
        async_push: m.get("async").map(|v| v != "off" && v != "false" && v != "0").unwrap_or(true),
        process_max: num("process_max", 4).clamp(1, 32) as usize,
        compress_level: m.get("compress_level").and_then(|v| v.parse().ok()).unwrap_or(1),
        prefetch: num("prefetch", 8).min(128) as usize,
        archive_timeout: num("archive_timeout", 60).max(5),
        socket_dir: m.get("socket_dir").cloned().unwrap_or("/var/run/postgresql".into()),
        port: m.get("port").and_then(|v| v.parse().ok()).unwrap_or(5432),
        bindir: m.get("bindir").cloned().unwrap_or_default(),
        retention_days: m.get("retention_days").and_then(|v| v.parse().ok()).unwrap_or(7),
    })
}

pub fn render_conf(c: &Conf) -> String {
    let mut s = String::from("# pgbx point-in-time restore settings; S3 keys are never stored here, only the path of the credentials file\n");
    let mut kv = |k: &str, v: String| {
        if !v.is_empty() {
            s.push_str(&format!("{k}={v}\n"));
        }
    };
    kv("s3_endpoint", c.s3.endpoint.clone());
    kv("s3_bucket", c.s3.bucket.clone());
    kv("s3_region", c.s3.region.clone());
    kv("credentials_file", c.s3.credentials_file.clone());
    kv("server_name", c.server.clone());
    kv("system_id", c.system_id.clone());
    kv("work_dir", c.work_dir.display().to_string());
    kv("pgdata", c.pgdata.clone());
    kv("wal_queue_max", c.queue_max.map(|v| v.to_string()).unwrap_or_default());
    kv("wal_segment_size", c.segment_size.to_string());
    kv("async", if c.async_push { "on".into() } else { "off".into() });
    kv("process_max", c.process_max.to_string());
    kv("compress_level", c.compress_level.to_string());
    kv("prefetch", c.prefetch.to_string());
    kv("archive_timeout", c.archive_timeout.to_string());
    kv("socket_dir", c.socket_dir.clone());
    kv("port", c.port.to_string());
    kv("bindir", c.bindir.clone());
    kv("retention_days", c.retention_days.to_string());
    s
}

pub fn load_conf(path: &Path) -> Result<Conf, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    parse_conf(&text, path)
}

/// --conf, else $PGBX_WAL_CONF, else <cwd>/../pgbx/pgbx-wal.conf (archive_command runs in the data directory and
/// the default pgbx.work_dir is <data_directory>/../pgbx).
pub fn conf_path(flag: Option<&str>) -> PathBuf {
    if let Some(f) = flag.filter(|f| !f.is_empty()) {
        return PathBuf::from(f);
    }
    if let Ok(e) = std::env::var("PGBX_WAL_CONF") {
        if !e.is_empty() {
            return PathBuf::from(e);
        }
    }
    std::env::current_dir().unwrap_or_default().join("../pgbx").join(CONF_NAME)
}

// ------------------------------------------------------------------------------------------------ names (pure)

pub fn is_segment(name: &str) -> bool {
    name.len() == 24 && name.chars().all(|c| c.is_ascii_hexdigit())
}

pub fn is_partial(name: &str) -> bool {
    name.len() == 32 && name.ends_with(".partial") && is_segment(&name[..24])
}

pub fn is_history(name: &str) -> bool {
    name.len() == 16 && name.ends_with(".history") && name[..8].chars().all(|c| c.is_ascii_hexdigit())
}

pub fn is_backup_file(name: &str) -> bool {
    name.len() == 40 && name.ends_with(".backup") && is_segment(&name[..24])
}

/// A file name Postgres archives (and only those: nothing with a path or odd characters gets near an S3 key).
pub fn archivable(name: &str) -> bool {
    is_segment(name) || is_partial(name) || is_history(name) || is_backup_file(name)
}

/// s3 key of an archived file.
pub fn wal_key(server: &str, system_id: &str, file: &str) -> String {
    format!("{server}/{system_id}/wal/{}/{file}.zst", &file[..8])
}

pub fn root(server: &str, system_id: &str) -> String {
    format!("{server}/{system_id}/")
}

/// The segment after `name` (same timeline), for a given segment size.
pub fn next_segment(name: &str, seg_size: u64) -> Option<String> {
    if !is_segment(name) {
        return None;
    }
    let tl = u32::from_str_radix(&name[..8], 16).ok()?;
    let log = u32::from_str_radix(&name[8..16], 16).ok()?;
    let seg = u32::from_str_radix(&name[16..24], 16).ok()?;
    let per_log = 0x1_0000_0000u64 / seg_size.max(1);
    let (log, seg) = if seg as u64 + 1 >= per_log { (log.checked_add(1)?, 0) } else { (log, seg + 1) };
    Some(format!("{tl:08X}{log:08X}{seg:08X}"))
}

/// Segment name holding an LSN ("0/3000028") on a timeline.
pub fn segment_of_lsn(tl: u32, lsn: &str, seg_size: u64) -> Option<String> {
    let v = parse_lsn(lsn)?;
    let segno = v / seg_size;
    let per_log = 0x1_0000_0000u64 / seg_size;
    Some(format!("{tl:08X}{:08X}{:08X}", segno / per_log, segno % per_log))
}

pub fn parse_lsn(lsn: &str) -> Option<u64> {
    let (hi, lo) = lsn.trim().split_once('/')?;
    Some((u64::from_str_radix(hi, 16).ok()? << 32) | u64::from_str_radix(lo, 16).ok()?)
}

/// System identifier and segment size from a WAL segment's long page header (first page).
/// XLogPageHeaderData is 24 bytes (magic u16, info u16, tli u32, pageaddr u64, rem_len u32, pad), then
/// XLogLongPageHeaderData adds sysid u64 @24, seg_size u32 @32, blcksz u32 @36. Native (little) endian.
pub fn segment_header(buf: &[u8]) -> Option<(u64, u32)> {
    if buf.len() < 40 {
        return None;
    }
    let info = u16::from_le_bytes([buf[2], buf[3]]);
    if info & 0x0002 == 0 {
        return None; // XLP_LONG_HEADER not set
    }
    let sysid = u64::from_le_bytes(buf[24..32].try_into().ok()?);
    let seg = u32::from_le_bytes(buf[32..36].try_into().ok()?);
    Some((sysid, seg))
}

/// System identifier from <pgdata>/global/pg_control (first field of ControlFileData).
pub fn control_system_id(pgdata: &Path) -> Option<u64> {
    let b = std::fs::read(pgdata.join("global/pg_control")).ok()?;
    (b.len() >= 8).then(|| u64::from_le_bytes(b[0..8].try_into().unwrap()))
}

pub fn sha256_hex(b: &[u8]) -> String {
    let d = Sha256::digest(b);
    d.iter().map(|x| format!("{x:02x}")).collect()
}

// ------------------------------------------------------------------------------------------------ push

#[derive(Debug, PartialEq)]
pub enum Pushed {
    Uploaded,
    AlreadyThere,
}

/// Archive one file: verify it belongs to this system, sha256, zstd, HEAD (idempotency), PUT if absent.
/// Same name + same checksum = success; same name + different checksum = error (never overwrite).
pub fn push_one(c: &Conf, b: &s3::Bucket, path: &Path, system_id: &str) -> Result<Pushed, String> {
    let name = path.file_name().and_then(|n| n.to_str()).ok_or("bad WAL path")?.to_string();
    if !archivable(&name) {
        return Err(format!("refusing to archive '{name}': not a WAL segment, .partial, .history or .backup file"));
    }
    let raw = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    if is_segment(&name) || is_partial(&name) {
        if let Some((sid, _)) = segment_header(&raw) {
            if sid.to_string() != system_id {
                return Err(format!("{name} belongs to database system {sid}, not {system_id}: refusing to archive it here"));
            }
        }
    }
    let sum = sha256_hex(&raw);
    let key = wal_key(&c.server, system_id, &name);
    let check = |h: s3x::Head| -> Result<Pushed, String> {
        match h.meta.get("sha256") {
            Some(s) if *s == sum => Ok(Pushed::AlreadyThere),
            Some(s) => Err(format!(
                "{name} is already archived with a DIFFERENT checksum ({s} in S3, {sum} here); refusing to overwrite. \
                 Two servers may be archiving into the same path (same server_name and system id): stop one of them"
            )),
            None => Err(format!("{name} exists in S3 without a checksum; refusing to overwrite it")),
        }
    };
    if let Some(h) = s3x::retry(&format!("check {key}"), s3x::SHORT, || s3x::head(b, &key))? {
        return check(h);
    }
    let z = zstd::bulk::compress(&raw, c.compress_level).map_err(|e| format!("compress {name}: {e}"))?;
    let size = raw.len().to_string();
    let created = s3x::retry(&format!("upload {key}"), s3x::SHORT, || {
        s3x::put_new(b, &key, &z, &[("sha256", &sum), ("size", &size)])
    })?;
    if created {
        return Ok(Pushed::Uploaded);
    }
    let h = s3x::retry(&format!("check {key}"), s3x::SHORT, || s3x::head(b, &key))?.ok_or(format!("{key}: conflict but not found"))?;
    check(h)
}

/// pg_wal/archive_status/*.ready -> file names, sorted.
pub fn ready_list(pg_wal: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(pg_wal.join("archive_status"))
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".ready")).map(String::from))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// Bytes of WAL waiting to be archived: ready segments x segment size (history/backup files are tiny).
pub fn queue_bytes(ready: &[String], seg_size: u64) -> u64 {
    ready.iter().filter(|n| is_segment(n) || is_partial(n)).count() as u64 * seg_size
}

/// Drop when the queue exceeds the max (pgBackRest archivePushDrop: strictly greater).
pub fn should_drop(queue: u64, max: Option<u64>) -> bool {
    max.is_some_and(|m| queue > m)
}

pub fn record_drop(work_dir: &Path, file: &str, queue: u64, max: u64) {
    let line = format!("{} {file} queue={queue} max={max}\n", chrono::Utc::now().timestamp());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(work_dir.join(DROPS_LOG)) {
        let _ = f.write_all(line.as_bytes());
    }
}

fn spool(c: &Conf, kind: &str) -> PathBuf {
    c.work_dir.join("spool").join(kind)
}

fn write_atomic(p: &Path, body: &[u8]) -> std::io::Result<()> {
    let tmp = p.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, p)
}

#[cfg(unix)]
mod lock {
    use std::os::unix::io::AsRawFd;
    pub struct Lock(#[allow(dead_code)] std::fs::File);
    /// Non-blocking exclusive lock on a file; None when someone else holds it.
    pub fn try_lock(p: &std::path::Path) -> Option<Lock> {
        let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(p).ok()?;
        let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        (r == 0).then_some(Lock(f))
    }
}
#[cfg(not(unix))]
mod lock {
    pub struct Lock;
    pub fn try_lock(_: &std::path::Path) -> Option<Lock> {
        Some(Lock)
    }
}

/// Start `pgbx <args>` detached (own session, no stdio) so it outlives the calling archive/restore command.
fn spawn_detached(args: &[String], cwd: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("current exe: {e}"))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args).current_dir(cwd).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    cmd.spawn().map(|_| ()).map_err(|e| format!("start background process: {e}"))
}

fn resolve_system_id(c: &Conf, pgdata: &Path) -> Result<String, String> {
    if let Some(s) = control_system_id(pgdata) {
        return Ok(s.to_string());
    }
    if !c.system_id.is_empty() {
        return Ok(c.system_id.clone());
    }
    Err("cannot read the system identifier (global/pg_control) and the conf has no system_id".into())
}

/// archive_command entry point. Ok = Postgres may consider the file archived.
pub fn wal_push(c: &Conf, wal_path: &str) -> Result<String, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let p = cwd.join(wal_path);
    let name = p.file_name().and_then(|n| n.to_str()).ok_or("bad WAL path")?.to_string();
    let pg_wal = p.parent().ok_or("bad WAL path")?.to_path_buf();
    let pgdata = pg_wal.parent().map(|x| x.to_path_buf()).unwrap_or(cwd.clone());
    if !c.async_push {
        let ready = ready_list(&pg_wal);
        let q = queue_bytes(&ready, c.segment_size);
        if should_drop(q, c.queue_max) {
            record_drop(&c.work_dir, &name, q, c.queue_max.unwrap_or(0));
            return Ok(format!("DROPPED {name}: archive queue {q} bytes exceeds wal_queue_max (gap recorded)"));
        }
        let sid = resolve_system_id(c, &pgdata)?;
        let b = s3x::bucket(&c.s3)?;
        return push_one(c, &b, &p, &sid).map(|r| format!("{name}: {r:?}"));
    }
    let sp = spool(c, "push");
    std::fs::create_dir_all(&sp).map_err(|e| format!("create spool {}: {e}", sp.display()))?;
    let ok = sp.join(format!("{name}.ok"));
    let err = sp.join(format!("{name}.error"));
    let deadline = Instant::now() + Duration::from_secs(c.archive_timeout);
    let mut forked = false;
    let mut sleep = Duration::from_millis(2);
    loop {
        if let Ok(msg) = std::fs::read_to_string(&ok) {
            let _ = std::fs::remove_file(&ok);
            return Ok(if msg.is_empty() { format!("{name}: pushed asynchronously") } else { msg });
        }
        if forked {
            if let Ok(e) = std::fs::read_to_string(&err) {
                return Err(e);
            }
        }
        if !forked {
            if let Some(l) = lock::try_lock(&c.work_dir.join("push.lock")) {
                let _ = std::fs::remove_file(&err);
                drop(l);
                let conf = conf_path_of(c);
                spawn_detached(&["wal-push".into(), "--async-daemon".into(), "--conf".into(), conf, wal_path.to_string()], &cwd)?;
            }
            forked = true;
        }
        if Instant::now() > deadline {
            return Err(format!(
                "unable to push {name} asynchronously within {}s; see {}",
                c.archive_timeout,
                err.display()
            ));
        }
        std::thread::sleep(sleep);
        sleep = (sleep * 2).min(Duration::from_millis(50));
    }
}

fn conf_path_of(c: &Conf) -> String {
    let p = if c.path.is_absolute() { c.path.clone() } else { std::env::current_dir().unwrap_or_default().join(&c.path) };
    p.display().to_string()
}

/// The background push process: one at a time (push.lock), pushes every ready file ahead of Postgres.
pub fn push_daemon(c: &Conf, wal_path: &str) -> Result<(), String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let p = cwd.join(wal_path);
    let pg_wal = p.parent().ok_or("bad WAL path")?.to_path_buf();
    let pgdata = pg_wal.parent().map(|x| x.to_path_buf()).unwrap_or(cwd.clone());
    let Some(_l) = lock::try_lock(&c.work_dir.join("push.lock")) else { return Ok(()) };
    let sp = spool(c, "push");
    let fail_all = |e: &str, files: &[String]| {
        for f in files {
            let _ = write_atomic(&sp.join(format!("{f}.error")), e.as_bytes());
        }
    };
    let sid = match resolve_system_id(c, &pgdata) {
        Ok(s) => s,
        Err(e) => {
            fail_all(&e, &ready_list(&pg_wal));
            return Err(e);
        }
    };
    let b = match s3x::bucket(&c.s3) {
        Ok(b) => b,
        Err(e) => {
            fail_all(&e, &ready_list(&pg_wal));
            return Err(e);
        }
    };
    let mut idle_since = Instant::now();
    loop {
        let ready = ready_list(&pg_wal);
        // forget statuses Postgres no longer needs (its .ready is gone)
        if let Ok(rd) = std::fs::read_dir(&sp) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                if let Some(f) = n.strip_suffix(".ok").or(n.strip_suffix(".error")) {
                    if ready.binary_search(&f.to_string()).is_err() {
                        let _ = std::fs::remove_file(e.path());
                    }
                }
            }
        }
        let todo: Vec<String> = ready.iter().filter(|f| !sp.join(format!("{f}.ok")).exists()).cloned().collect();
        if todo.is_empty() {
            if idle_since.elapsed() > Duration::from_secs(3) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        idle_since = Instant::now();
        let q = queue_bytes(&ready, c.segment_size);
        if should_drop(q, c.queue_max) {
            for f in &todo {
                record_drop(&c.work_dir, f, q, c.queue_max.unwrap_or(0));
                let _ = write_atomic(
                    &sp.join(format!("{f}.ok")),
                    format!("DROPPED {f}: archive queue {q} bytes exceeds wal_queue_max (gap recorded)").as_bytes(),
                );
            }
            continue;
        }
        let batch: Vec<String> = todo.into_iter().take(c.process_max * 8).collect();
        let next = std::sync::atomic::AtomicUsize::new(0);
        let failed = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|s| {
            for _ in 0..c.process_max.min(batch.len()) {
                s.spawn(|| loop {
                    use std::sync::atomic::Ordering;
                    if failed.load(Ordering::Relaxed) {
                        return;
                    }
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    let Some(f) = batch.get(i) else { return };
                    match push_one(c, &b, &pg_wal.join(f), &sid) {
                        Ok(r) => {
                            let _ = write_atomic(&sp.join(format!("{f}.ok")), format!("{f}: {r:?}").as_bytes());
                        }
                        Err(e) => {
                            failed.store(true, Ordering::Relaxed);
                            let _ = write_atomic(&sp.join(format!("{f}.error")), e.as_bytes());
                        }
                    }
                });
            }
        });
        if failed.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("a push failed; the next archive_command call starts a new run".into());
        }
    }
}

// ------------------------------------------------------------------------------------------------ get

/// Fetch one archived file and verify it. Ok(None) = not in the archive.
pub fn fetch(c: &Conf, b: &s3::Bucket, name: &str) -> Result<Option<Vec<u8>>, String> {
    let key = wal_key(&c.server, &c.system_id, name);
    let Some((z, meta)) = s3x::retry(&format!("download {key}"), s3x::SHORT, || s3x::get_meta(b, &key))? else { return Ok(None) };
    let cap = if is_segment(name) || is_partial(name) { c.segment_size as usize + 1 } else { 64 << 20 };
    let raw = zstd::bulk::decompress(&z, cap).map_err(|e| format!("decompress {name}: {e}"))?;
    let got = sha256_hex(&raw);
    match meta.get("sha256") {
        Some(want) if *want == got => Ok(Some(raw)),
        Some(want) => Err(format!("{name}: checksum mismatch after download (S3 says {want}, got {got}); not using it")),
        None => Err(format!("{name}: archived without a checksum; not using it")),
    }
}

/// restore_command entry point. Ok(true) = delivered, Ok(false) = not in the archive (exit 1, normal at the end).
pub fn wal_get(c: &Conf, name: &str, dest: &str) -> Result<bool, String> {
    if !archivable(name) && !name.ends_with(".history") {
        return Ok(false);
    }
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let dest = cwd.join(dest);
    let sp = spool(c, "get");
    let cached = sp.join(name);
    if cached.exists() && std::fs::rename(&cached, &dest).is_ok() {
        kick_prefetch(c, name, &cwd);
        return Ok(true);
    }
    let b = s3x::bucket(&c.s3)?;
    let Some(raw) = fetch(c, &b, name)? else { return Ok(false) };
    let tmp = dest.with_extension("pgbx-tmp");
    std::fs::write(&tmp, &raw).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &dest).map_err(|e| format!("rename into {}: {e}", dest.display()))?;
    kick_prefetch(c, name, &cwd);
    Ok(true)
}

fn kick_prefetch(c: &Conf, name: &str, cwd: &Path) {
    if c.prefetch == 0 || !is_segment(name) {
        return;
    }
    // already prefetched far enough ahead? (the last segment of the window is in the spool)
    let mut last = name.to_string();
    for _ in 0..c.prefetch / 2 {
        match next_segment(&last, c.segment_size) {
            Some(n) => last = n,
            None => return,
        }
    }
    if spool(c, "get").join(&last).exists() {
        return;
    }
    if lock::try_lock(&c.work_dir.join("get.lock")).is_none() {
        return; // one is running
    }
    let _ = spawn_detached(&["wal-get".into(), "--prefetch-daemon".into(), "--conf".into(), conf_path_of(c), name.to_string(), "-".into()], cwd);
}

/// Segments to prefetch after `name`: the next `n` on the same timeline.
pub fn prefetch_list(name: &str, n: usize, seg_size: u64) -> Vec<String> {
    let mut v = vec![];
    let mut cur = name.to_string();
    for _ in 0..n {
        match next_segment(&cur, seg_size) {
            Some(x) => {
                v.push(x.clone());
                cur = x;
            }
            None => break,
        }
    }
    v
}

/// Background: fetch the next `prefetch` segments after `name` in parallel into the spool; stop at the end of
/// the archive. Spool files older than `name` are removed (replay is past them).
pub fn prefetch_daemon(c: &Conf, name: &str) -> Result<(), String> {
    let Some(_l) = lock::try_lock(&c.work_dir.join("get.lock")) else { return Ok(()) };
    let sp = spool(c, "get");
    std::fs::create_dir_all(&sp).map_err(|e| e.to_string())?;
    if let Ok(rd) = std::fs::read_dir(&sp) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if (is_segment(&n) && n.as_str() <= name && n[..8] == name[..8]) || n.contains("pgbx-tmp") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let b = s3x::bucket(&c.s3)?;
    let want: Vec<String> = prefetch_list(name, c.prefetch, c.segment_size).into_iter().filter(|f| !sp.join(f).exists()).collect();
    let missing_from = std::sync::Mutex::new(None::<usize>);
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..c.process_max.min(want.len()) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let Some(f) = want.get(i) else { return };
                if missing_from.lock().unwrap().is_some_and(|m| i > m) {
                    return;
                }
                match fetch(c, &b, f) {
                    Ok(Some(raw)) => {
                        let tmp = sp.join(format!("{f}.pgbx-tmp"));
                        if std::fs::write(&tmp, &raw).is_ok() {
                            let _ = std::fs::rename(&tmp, sp.join(f));
                        }
                    }
                    _ => {
                        let mut m = missing_from.lock().unwrap();
                        *m = Some(m.map_or(i, |x| x.min(i)));
                    }
                }
            });
        }
    });
    Ok(())
}

#[cfg(test)]
mod t {
    use super::*;

    #[test]
    fn key_layout() {
        assert_eq!(wal_key("prod", "7300", "000000010000000000000003"), "prod/7300/wal/00000001/000000010000000000000003.zst");
        assert_eq!(wal_key("prod", "7300", "00000002.history"), "prod/7300/wal/00000002/00000002.history.zst");
        assert_eq!(
            wal_key("prod", "7300", "000000010000000000000003.00000028.backup"),
            "prod/7300/wal/00000001/000000010000000000000003.00000028.backup.zst"
        );
    }

    #[test]
    fn names() {
        assert!(archivable("000000010000000000000003"));
        assert!(archivable("00000002.history"));
        assert!(archivable("000000010000000000000003.00000028.backup"));
        assert!(archivable("000000010000000000000003.partial"));
        assert!(!archivable("../../etc/passwd"));
        assert!(!archivable("00000001000000000000000G"));
        assert!(!archivable("RECOVERYXLOG"));
    }

    #[test]
    fn segment_math() {
        let s16 = 16 << 20;
        assert_eq!(next_segment("0000000100000000000000FE", s16).unwrap(), "0000000100000000000000FF");
        assert_eq!(next_segment("0000000100000000000000FF", s16).unwrap(), "000000010000000100000000");
        assert_eq!(next_segment("00000001000000000000003F", 64 << 20).unwrap(), "000000010000000100000000");
        assert_eq!(segment_of_lsn(1, "0/3000028", s16).unwrap(), "000000010000000000000003");
        assert_eq!(segment_of_lsn(2, "1/A0000000", s16).unwrap(), "0000000200000001000000A0");
        assert_eq!(prefetch_list("000000010000000000000003", 2, s16), vec!["000000010000000000000004", "000000010000000000000005"]);
    }

    #[test]
    fn header() {
        let mut b = vec![0u8; 64];
        b[2] = 0x02 | 0x04; // XLP_LONG_HEADER | XLP_BKP_REMOVABLE
        b[24..32].copy_from_slice(&7300u64.to_le_bytes());
        b[32..36].copy_from_slice(&(16u32 << 20).to_le_bytes());
        assert_eq!(segment_header(&b), Some((7300, 16 << 20)));
        b[2] = 0;
        assert_eq!(segment_header(&b), None);
    }

    #[test]
    fn queue_and_drops() {
        let ready: Vec<String> = vec!["000000010000000000000003".into(), "000000010000000000000004".into(), "00000002.history".into()];
        assert_eq!(queue_bytes(&ready, 16 << 20), 32 << 20);
        assert!(!should_drop(32 << 20, Some(32 << 20)));
        assert!(should_drop((32 << 20) + 1, Some(32 << 20)));
        assert!(!should_drop(u64::MAX, None));
    }

    #[test]
    fn conf_roundtrip() {
        let c = Conf {
            s3: s3x::S3Conf { endpoint: "http://s3:9000".into(), bucket: "b".into(), region: "r".into(), credentials_file: "/etc/pgbx/s3.credentials".into() },
            server: "prod".into(),
            system_id: "7300".into(),
            work_dir: "/var/lib/postgresql/pgbx".into(),
            queue_max: Some(4 << 30),
            segment_size: 16 << 20,
            async_push: true,
            process_max: 4,
            compress_level: 1,
            prefetch: 8,
            archive_timeout: 60,
            port: 5432,
            retention_days: 7,
            ..Default::default()
        };
        let text = render_conf(&c);
        assert!(!text.contains("secret") && !text.contains("access_key"));
        let back = parse_conf(&text, Path::new("/x/pgbx-wal.conf")).unwrap();
        assert_eq!(back.server, "prod");
        assert_eq!(back.queue_max, Some(4 << 30));
        assert_eq!(back.work_dir, PathBuf::from("/var/lib/postgresql/pgbx"));
        assert!(back.async_push);
        assert!(parse_conf("s3_bucket=b\n", Path::new("/x/c")).unwrap_err().contains("s3_endpoint"));
    }
}
