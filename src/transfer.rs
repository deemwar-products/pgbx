//! Robust S3 transfers for per-database dumps.
//! Upload: our own multipart loop in fixed-size parts; each part stays in memory until S3 confirms it, so a failed
//! part is retried with backoff while memory stays bounded at one part, whatever the database size.
//! Download: if the connection drops, continue from the byte we reached (HTTP Range) into the same writer,
//! so a running pg_restore never restarts from zero.

use crate::worker::{log, shutting_down};
use s3::Bucket;
use std::io::{Read, Write};
use std::time::Duration;

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
                    return Err(format!("{what}: stopped, Postgres is shutting down"));
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        if shutting_down() {
            return Err(format!("{what}: stopped, Postgres is shutting down"));
        }
        match f() {
            Ok(v) => return Ok(v),
            Err(e) => last = e,
        }
    }
    Err(format!("{what}: gave up after {} retries: {last}", BACKOFF_SECS.len()))
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

/// Stream `r` to `key`. Small inputs go up in one PUT; larger ones as a multipart upload with per-part retries.
/// On failure the multipart upload is aborted, so nothing half-written is left looking like a backup.
pub fn upload_stream(b: &Bucket, key: &str, r: &mut impl Read) -> Result<u64, String> {
    let mut buf = vec![0u8; PART_SIZE];
    let n = fill(r, &mut buf).map_err(|e| format!("read dump: {e}"))?;
    if n < PART_SIZE {
        retry(&format!("upload {key}"), || {
            let resp = b.put_object(key, &buf[..n]).map_err(|e| e.to_string())?;
            if resp.status_code() / 100 == 2 { Ok(()) } else { Err(format!("HTTP {}", resp.status_code())) }
        })?;
        return Ok(n as u64);
    }
    let id = retry(&format!("start upload {key}"), || b.initiate_multipart_upload(key, CT).map_err(|e| e.to_string()))?.upload_id;
    let result = (|| {
        let mut parts = Vec::new();
        let mut total = 0u64;
        let mut len = n;
        let mut number = 1u32;
        loop {
            let chunk = &buf[..len];
            let part = retry(&format!("upload {key} part {number}"), || {
                b.put_multipart_chunk(chunk, key, number, &id, CT).map_err(|e| e.to_string())
            })?;
            parts.push(part);
            total += len as u64;
            len = fill(r, &mut buf).map_err(|e| format!("read dump: {e}"))?;
            if len == 0 {
                break;
            }
            number += 1;
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
    result
}

/// Writer wrapper that remembers whether the *destination* failed (pg_restore died) vs the network.
struct Tracked<'a, W: Write> {
    inner: &'a mut W,
    written: &'a mut u64,
    dest_failed: &'a mut bool,
}
impl<W: Write> Write for Tracked<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if shutting_down() {
            *self.dest_failed = true;
            return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "Postgres is shutting down"));
        }
        match self.inner.write(buf) {
            Ok(k) => {
                *self.written += k as u64;
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

/// Download `key` into `w`, resuming from the last byte on network errors. Destination errors are not retried.
pub fn download_resumable<W: Write + Send>(b: &Bucket, key: &str, w: &mut W) -> Result<u64, String> {
    let size = retry(&format!("stat {key}"), || {
        let (head, code) = b.head_object(key).map_err(|e| e.to_string())?;
        if code / 100 != 2 {
            return Err(format!("HTTP {code}"));
        }
        head.content_length.map(|n| n as u64).ok_or("no content length".to_string())
    })?;
    let mut done = 0u64;
    let mut dest_failed = false;
    retry(&format!("download {key}"), || {
        if done >= size {
            return Ok(());
        }
        if dest_failed {
            return Ok(()); // reported below, not retryable
        }
        let from = done;
        let mut t = Tracked { inner: w, written: &mut done, dest_failed: &mut dest_failed };
        let r = b.get_object_range_to_writer(key, from, None, &mut t);
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
