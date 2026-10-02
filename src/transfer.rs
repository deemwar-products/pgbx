//! Robust S3 transfers for per-database dumps.
//! Upload: our own multipart loop in fixed-size parts; each part stays in memory until S3 confirms it, so a failed
//! part is retried with backoff while memory stays bounded at one part, whatever the database size.
//! Download: if the connection drops, continue from the byte we reached (HTTP Range) into the same writer,
//! so a running pg_restore never restarts from zero.

use crate::worker::{log, shutting_down, stop_reason};
use s3::Bucket;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Network throughput of the last 20 upload parts / download stretches (bytes per second, time on the wire only:
/// bandwidth-cap sleeps excluded). Read by the worker for the time estimates (ADR 0001 §4); no extra traffic.
static UPLOAD_RATES: Mutex<VecDeque<f64>> = Mutex::new(VecDeque::new());
static DOWNLOAD_RATES: Mutex<VecDeque<f64>> = Mutex::new(VecDeque::new());

fn sample(q: &Mutex<VecDeque<f64>>, bytes: u64, secs: f64) {
    if bytes < 1024 * 1024 || secs <= 0.0 {
        return; // too small to say anything about the network
    }
    let mut q = q.lock().unwrap_or_else(|e| e.into_inner());
    q.push_back(bytes as f64 / secs);
    while q.len() > 20 {
        q.pop_front();
    }
}

pub fn median(v: &[f64]) -> Option<f64> {
    let mut v: Vec<f64> = v.iter().copied().filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let m = v.len() / 2;
    Some(if v.len() % 2 == 1 { v[m] } else { (v[m - 1] + v[m]) / 2.0 })
}

/// (upload, download) median bytes/s of the recent samples.
pub fn network_rates() -> (Option<f64>, Option<f64>) {
    let get = |q: &Mutex<VecDeque<f64>>| median(&q.lock().unwrap_or_else(|e| e.into_inner()).iter().copied().collect::<Vec<_>>());
    (get(&UPLOAD_RATES), get(&DOWNLOAD_RATES))
}

pub const PART_SIZE: usize = 16 * 1024 * 1024;
const BACKOFF_SECS: [u64; 8] = [1, 2, 4, 8, 16, 30, 30, 30];
const CT: &str = "application/octet-stream";

/// Run `f` until it succeeds, sleeping between attempts. Gives up after the backoff table (~2 minutes).
pub fn retry<T>(what: &str, mut f: impl FnMut() -> Result<T, String>) -> Result<T, String> {
    let mut last = String::new();
    for (i, wait) in std::iter::once(0).chain(BACKOFF_SECS).enumerate() {
        if wait > 0 {
            log(&format!("{what}: retry {i}/{} in {wait}s after: {last}", BACKOFF_SECS.len()));
            for _ in 0..wait * 5 {
                if shutting_down() {
                    return Err(format!("{what}: stopped, {}", stop_reason()));
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        if shutting_down() {
            return Err(format!("{what}: stopped, {}", stop_reason()));
        }
        match f() {
            Ok(v) => return Ok(v),
            Err(e) => last = e,
        }
    }
    Err(format!("{what}: gave up after {} retries: {last}", BACKOFF_SECS.len()))
}

/// Token bucket for pgbx.upload_kbps / download_kbps (KiB/s, 0 = unlimited). Holds at most one second of tokens;
/// a chunk larger than that goes into debt, and the caller sleeps until the debt is paid back.
pub struct Throttle {
    rate: f64, // bytes per second; 0 = unlimited
    tokens: f64,
    at: Option<Instant>,
    slept: Duration, // total time spent pacing (kept out of the network samples)
}

impl Throttle {
    pub fn new(kbps: i32) -> Self {
        let rate = kbps.max(0) as f64 * 1024.0;
        Throttle { rate, tokens: rate, at: None, slept: Duration::ZERO }
    }

    /// Take `n` bytes at `now`; returns how long to wait before sending more.
    pub fn take(&mut self, n: usize, now: Instant) -> Duration {
        if self.rate <= 0.0 {
            return Duration::ZERO;
        }
        if let Some(t) = self.at {
            self.tokens = (self.tokens + now.saturating_duration_since(t).as_secs_f64() * self.rate).min(self.rate);
        }
        self.at = Some(now);
        self.tokens -= n as f64;
        if self.tokens >= 0.0 { Duration::ZERO } else { Duration::from_secs_f64(-self.tokens / self.rate) }
    }

    /// take() and sleep it off; wakes every 200 ms to honour a shutdown.
    pub fn pace(&mut self, n: usize) {
        let t0 = Instant::now();
        let until = Instant::now() + self.take(n, Instant::now());
        while !shutting_down() {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            std::thread::sleep(left.min(Duration::from_millis(200)));
        }
        self.slept += t0.elapsed();
    }
}

/// Fill `buf` from `r` until full or EOF; returns bytes read.
fn fill(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// Stream `r` to `key` at most `kbps` KiB/s (0 = unlimited). Small inputs go up in one PUT; larger ones as a
/// multipart upload with per-part retries. On failure the multipart upload is aborted, so nothing half-written is
/// left looking like a backup. A stop (shutdown or cancel) before the last request never completes the upload.
/// `progress` gets the bytes uploaded so far after each part.
pub fn upload_stream(b: &Bucket, key: &str, r: &mut impl Read, kbps: i32, progress: &mut dyn FnMut(u64)) -> Result<u64, String> {
    let mut buf = vec![0u8; PART_SIZE];
    let mut throttle = Throttle::new(kbps);
    let n = fill(r, &mut buf).map_err(|e| format!("read dump: {e}"))?;
    if n < PART_SIZE {
        throttle.pace(n);
        if shutting_down() {
            return Err(format!("upload {key}: stopped, {}", stop_reason()));
        }
        let t0 = Instant::now();
        retry(&format!("upload {key}"), || {
            let resp = b.put_object(key, &buf[..n]).map_err(|e| e.to_string())?;
            if resp.status_code() / 100 == 2 { Ok(()) } else { Err(format!("HTTP {}", resp.status_code())) }
        })?;
        sample(&UPLOAD_RATES, n as u64, t0.elapsed().as_secs_f64());
        progress(n as u64);
        return Ok(n as u64);
    }
    let id = retry(&format!("start upload {key}"), || b.initiate_multipart_upload(key, CT).map_err(|e| e.to_string()))?.upload_id;
    remember_upload(key, &id); // a crash from here on leaves an upload the next worker start aborts
    let result = (|| {
        let mut parts = Vec::new();
        let mut total = 0u64;
        let mut len = n;
        let mut number = 1u32;
        loop {
            let chunk = &buf[..len];
            throttle.pace(len); // meanwhile the pipe fills up, which slows pg_dump down
            let t0 = Instant::now();
            let part = retry(&format!("upload {key} part {number}"), || {
                b.put_multipart_chunk(chunk, key, number, &id, CT).map_err(|e| e.to_string())
            })?;
            sample(&UPLOAD_RATES, len as u64, t0.elapsed().as_secs_f64());
            parts.push(part);
            total += len as u64;
            progress(total);
            len = fill(r, &mut buf).map_err(|e| format!("read dump: {e}"))?;
            if len == 0 {
                break;
            }
            number += 1;
        }
        if shutting_down() {
            return Err(format!("upload {key}: stopped, {}", stop_reason()));
        }
        retry(&format!("finish upload {key}"), || {
            let resp = b.complete_multipart_upload(key, &id, parts.clone()).map_err(|e| e.to_string())?;
            if resp.status_code() / 100 == 2 { Ok(()) } else { Err(format!("HTTP {}", resp.status_code())) }
        })?;
        Ok(total)
    })();
    if result.is_err() {
        let _ = b.abort_upload(key, &id);
    }
    forget_upload(&id);
    result
}

/// Multipart uploads this server started and has not finished, one "key<TAB>upload id" per line, in the data
/// directory (the worker's working directory). Listing open uploads through S3 is not reliable across S3
/// implementations (some omit fields the client requires), so the worker keeps its own list.
const OPEN_UPLOADS: &str = "pgbx_open_uploads";
static OPEN_LOCK: Mutex<()> = Mutex::new(());

fn remember_upload(key: &str, id: &str) {
    let _g = OPEN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let r = std::fs::OpenOptions::new().create(true).append(true).open(OPEN_UPLOADS).and_then(|mut f| writeln!(f, "{key}\t{id}"));
    if let Err(e) = r {
        log(&format!("{OPEN_UPLOADS}: {e}"));
    }
}

fn forget_upload(id: &str) {
    let _g = OPEN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(text) = std::fs::read_to_string(OPEN_UPLOADS) else { return };
    let keep: String = text.lines().filter(|l| l.split('\t').nth(1) != Some(id)).map(|l| format!("{l}\n")).collect();
    let _ = if keep.is_empty() { std::fs::remove_file(OPEN_UPLOADS) } else { std::fs::write(OPEN_UPLOADS, keep) };
}

/// At worker start: abort every upload a crash left in the list (they are never a backup and cost storage).
pub fn abort_remembered(b: &Bucket) {
    let _g = OPEN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(text) = std::fs::read_to_string(OPEN_UPLOADS) else { return };
    let mut left = String::new();
    for l in text.lines() {
        let Some((key, id)) = l.split_once('\t') else { continue };
        match b.abort_upload(key, id) {
            Ok(_) => log(&format!("aborted orphaned upload {key}")),
            Err(e) if e.to_string().contains("NoSuchUpload") || e.to_string().contains("404") => {}
            Err(e) => {
                log(&format!("abort orphaned upload {key}: {e}; retried at the next start"));
                left.push_str(&format!("{l}\n"));
            }
        }
    }
    let _ = if left.is_empty() { std::fs::remove_file(OPEN_UPLOADS) } else { std::fs::write(OPEN_UPLOADS, left) };
}

/// Writer wrapper that remembers whether the *destination* failed (pg_restore died) vs the network.
struct Tracked<'a, W: Write> {
    inner: &'a mut W,
    written: &'a mut u64,
    dest_failed: &'a mut bool,
    throttle: &'a mut Throttle,
    progress: &'a mut (dyn FnMut(u64) + Send),
    reported: &'a mut u64,
}
impl<W: Write> Write for Tracked<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if shutting_down() {
            *self.dest_failed = true;
            // not Interrupted: write_all retries that forever and the shutdown would hang
            return Err(std::io::Error::other(stop_reason()));
        }
        match self.inner.write(buf) {
            Ok(k) => {
                *self.written += k as u64;
                self.throttle.pace(k);
                if *self.written >= *self.reported + PART_SIZE as u64 {
                    *self.reported = *self.written;
                    (self.progress)(*self.written);
                }
                Ok(k)
            }
            Err(e) => {
                *self.dest_failed = true;
                Err(e)
            }
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Download `key` into `w` at most `kbps` KiB/s (0 = unlimited), resuming from the last byte on network errors.
/// Destination errors are not retried. `progress` gets the bytes written so far, every part size.
pub fn download_resumable<W: Write + Send>(
    b: &Bucket, key: &str, w: &mut W, kbps: i32, progress: &mut (dyn FnMut(u64) + Send),
) -> Result<u64, String> {
    let size = retry(&format!("stat {key}"), || {
        let (head, code) = b.head_object(key).map_err(|e| e.to_string())?;
        if code / 100 != 2 {
            return Err(format!("HTTP {code}"));
        }
        head.content_length.map(|n| n as u64).ok_or("no content length".to_string())
    })?;
    let mut done = 0u64;
    let mut dest_failed = false;
    let mut throttle = Throttle::new(kbps);
    let mut reported = 0u64;
    retry(&format!("download {key}"), || {
        if done >= size {
            return Ok(());
        }
        if dest_failed {
            return Ok(()); // reported below, not retryable
        }
        let from = done;
        let (t0, slept0) = (Instant::now(), throttle.slept);
        let mut t = Tracked {
            inner: w, written: &mut done, dest_failed: &mut dest_failed, throttle: &mut throttle,
            progress: &mut *progress, reported: &mut reported,
        };
        let r = b.get_object_range_to_writer(key, from, None, &mut t);
        let wire = t0.elapsed().saturating_sub(throttle.slept - slept0);
        sample(&DOWNLOAD_RATES, done - from, wire.as_secs_f64());
        if dest_failed {
            return Ok(());
        }
        match r {
            Ok(code) if code / 100 == 2 && done >= size => Ok(()),
            Ok(code) if code / 100 == 2 => Err(format!("connection ended at {done}/{size} bytes")),
            Ok(code) => Err(format!("HTTP {code} at {done}/{size} bytes")),
            Err(e) => Err(format!("{e} at {done}/{size} bytes")),
        }
    })?;
    if dest_failed {
        return Err(format!("restore process stopped reading after {done} bytes"));
    }
    Ok(done)
}

/// Abort multipart uploads under `prefix` that a crash left behind (they cost storage and are never a backup).
pub fn abort_orphans(b: &Bucket, prefix: &str) {
    match b.list_multiparts_uploads(Some(prefix), None) {
        Ok(pages) => {
            for page in pages {
                for u in page.uploads {
                    if b.abort_upload(&u.key, &u.id).is_ok() {
                        log(&format!("aborted orphaned upload {}", u.key));
                    }
                }
            }
        }
        Err(e) => log(&format!("list orphaned uploads: {e}")),
    }
}

#[cfg(test)]
mod t {
    use super::*;

    #[test]
    fn medians() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[4.0, 1.0, 2.0, 3.0]), Some(2.5));
        assert_eq!(median(&[f64::NAN, 5.0]), Some(5.0));
    }

    #[test]
    fn throttle_off_never_waits() {
        let mut t = Throttle::new(0);
        assert_eq!(t.take(PART_SIZE * 100, Instant::now()), Duration::ZERO);
    }

    #[test]
    fn throttle_burst_then_debt() {
        let t0 = Instant::now();
        let mut t = Throttle::new(1000); // 1,024,000 B/s, bucket starts full (1 s)
        assert_eq!(t.take(512_000, t0), Duration::ZERO);
        assert_eq!(t.take(1_024_000, t0), Duration::from_millis(500)); // 512,000 B in debt
        // 0.5 s later the debt is paid, then a full second refills (never more than one second banked)
        assert_eq!(t.take(0, t0 + Duration::from_millis(500)), Duration::ZERO);
        assert_eq!(t.take(1_024_000, t0 + Duration::from_secs(10)), Duration::ZERO);
        assert_eq!(t.take(1_024, t0 + Duration::from_secs(10)), Duration::from_millis(1));
    }

    #[test]
    fn throttle_long_run_rate() {
        // 16 MiB parts at 10 MiB/s: 100 parts take 160 s, less the one second banked at the start
        let mut t = Throttle::new(10 * 1024);
        let mut now = Instant::now();
        let start = now;
        for _ in 0..100 {
            now += t.take(PART_SIZE, now);
        }
        let secs = (now - start).as_secs_f64();
        assert!((secs - 159.0).abs() < 0.01, "{secs}");
    }
}
