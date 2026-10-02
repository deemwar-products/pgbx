//! S3 transfer engine for point-in-time restore (WAL segments and base backups).
//!
//! Design ported from pgBackRest (MIT, see NOTICE): small objects in one PUT; large streams as a multipart upload
//! whose parts go up in PARALLEL (bounded memory: at most `2 x concurrency` parts in flight), each part retried
//! with backoff, the upload aborted on failure so nothing half-written looks like a backup; downloads fetch
//! ranges in parallel and write them in order, retrying a range on network errors.
//!
//! Portions Copyright (c) 2013-2026, David Steele (pgBackRest), MIT License.

use s3::{creds::Credentials, Bucket, Region};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::Duration;

pub const PART: usize = 16 * 1024 * 1024;
const CT: &str = "application/octet-stream";

#[derive(Clone, Debug, Default)]
pub struct S3Conf {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub credentials_file: String,
}

/// (access_key_id, secret_access_key) from a credentials file. Values are never printed.
pub fn read_credentials(file: &str) -> Result<(String, String), String> {
    let text = std::fs::read_to_string(file).map_err(|e| format!("read credentials file {file}: {e}"))?;
    crate::s3restore::parse_credentials(&text)
}

pub fn bucket(c: &S3Conf) -> Result<Box<Bucket>, String> {
    let (ak, sk) = read_credentials(&c.credentials_file)?;
    let creds = Credentials::new(Some(&ak), Some(&sk), None, None, None).map_err(|e| format!("credentials: {e}"))?;
    let region = if c.region.is_empty() { "us-east-1".to_string() } else { c.region.clone() };
    let mut b = Bucket::new(&c.bucket, Region::Custom { region, endpoint: c.endpoint.clone() }, creds)
        .map_err(|e| format!("bucket: {e}"))?
        .with_path_style();
    b.set_request_timeout(Some(Duration::from_secs(120)));
    Ok(b)
}

/// Run `f` until it succeeds, waiting `waits` seconds between attempts.
pub fn retry<T>(what: &str, waits: &[u64], mut f: impl FnMut() -> Result<T, String>) -> Result<T, String> {
    let mut last = String::new();
    for (i, w) in std::iter::once(&0u64).chain(waits.iter()).enumerate() {
        if *w > 0 {
            std::thread::sleep(Duration::from_secs(*w));
        }
        match f() {
            Ok(v) => return Ok(v),
            Err(e) => {
                last = e;
                if i == waits.len() {
                    break;
                }
            }
        }
    }
    Err(format!("{what}: gave up after {} attempts: {last}", waits.len() + 1))
}

/// Backoff for long transfers (base backups, restores): ~2 minutes total.
pub const LONG: &[u64] = &[1, 2, 4, 8, 16, 30, 30, 30];
/// Backoff inside archive_command / restore_command: Postgres retries the command itself.
pub const SHORT: &[u64] = &[1, 2];

pub struct Head {
    pub size: u64,
    pub meta: std::collections::HashMap<String, String>,
}

/// HEAD an object: Ok(None) when it does not exist.
pub fn head(b: &Bucket, key: &str) -> Result<Option<Head>, String> {
    match b.head_object(key) {
        Ok((_, 404)) => Ok(None),
        Ok((h, code)) if code / 100 == 2 => Ok(Some(Head {
            size: h.content_length.unwrap_or(0).max(0) as u64,
            meta: h.metadata.unwrap_or_default().into_iter().map(|(k, v)| (k.to_ascii_lowercase(), v)).collect(),
        })),
        Ok((_, code)) => Err(format!("HEAD {key}: HTTP {code}")),
        Err(e) => {
            let s = e.to_string();
            if s.contains("404") { Ok(None) } else { Err(format!("HEAD {key}: {s}")) }
        }
    }
}

/// PUT unless the key already exists (If-None-Match: * where the store supports it). Ok(false) = it existed.
pub fn put_new(b: &Bucket, key: &str, body: &[u8], meta: &[(&str, &str)]) -> Result<bool, String> {
    let mut req = b.put_object_builder(key, body).with_content_type(CT);
    for (k, v) in meta {
        req = req.with_metadata(*k, *v).map_err(|e| e.to_string())?;
    }
    req = req.with_header("If-None-Match", "*").map_err(|e| e.to_string())?;
    match req.execute() {
        Ok(r) if r.status_code() / 100 == 2 => Ok(true),
        Ok(r) if r.status_code() == 412 || r.status_code() == 409 => Ok(false),
        Ok(r) => Err(format!("PUT {key}: HTTP {}", r.status_code())),
        Err(e) => {
            let s = e.to_string();
            if s.contains("412") || s.contains("PreconditionFailed") { Ok(false) } else { Err(format!("PUT {key}: {s}")) }
        }
    }
}

pub fn put(b: &Bucket, key: &str, body: &[u8]) -> Result<(), String> {
    let r = b.put_object(key, body).map_err(|e| format!("PUT {key}: {e}"))?;
    if r.status_code() / 100 == 2 { Ok(()) } else { Err(format!("PUT {key}: HTTP {}", r.status_code())) }
}

/// GET a whole (small) object. Ok(None) when it does not exist.
pub fn get(b: &Bucket, key: &str) -> Result<Option<Vec<u8>>, String> {
    Ok(get_meta(b, key)?.map(|(v, _)| v))
}

/// An object's body and its x-amz-meta-* values.
pub type WithMeta = (Vec<u8>, std::collections::HashMap<String, String>);

/// GET a whole object with its x-amz-meta-* values (lower-case names, prefix stripped).
pub fn get_meta(b: &Bucket, key: &str) -> Result<Option<WithMeta>, String> {
    match b.get_object(key) {
        Ok(r) if r.status_code() == 404 => Ok(None),
        Ok(r) if r.status_code() / 100 == 2 => {
            let meta = r
                .headers()
                .into_iter()
                .filter_map(|(k, v)| k.to_ascii_lowercase().strip_prefix("x-amz-meta-").map(|k| (k.to_string(), v)))
                .collect();
            Ok(Some((r.to_vec(), meta)))
        }
        Ok(r) => Err(format!("GET {key}: HTTP {}", r.status_code())),
        Err(e) => {
            let s = e.to_string();
            if s.contains("404") || s.contains("NoSuchKey") { Ok(None) } else { Err(format!("GET {key}: {s}")) }
        }
    }
}

/// Every key under `prefix` (with sizes).
pub fn list(b: &Bucket, prefix: &str) -> Result<Vec<(String, u64)>, String> {
    Ok(b
        .list(prefix.to_string(), None)
        .map_err(|e| format!("list {prefix}: {e}"))?
        .into_iter()
        .flat_map(|p| p.contents.into_iter().map(|o| (o.key, o.size)))
        .collect())
}

/// Immediate "folders" under `prefix` (delimiter '/').
pub fn list_dirs(b: &Bucket, prefix: &str) -> Result<Vec<String>, String> {
    let mut out = vec![];
    for p in b.list(prefix.to_string(), Some("/".to_string())).map_err(|e| format!("list {prefix}: {e}"))? {
        for c in p.common_prefixes.unwrap_or_default() {
            out.push(c.prefix);
        }
    }
    Ok(out)
}

pub fn delete(b: &Bucket, key: &str) -> Result<(), String> {
    retry(&format!("delete {key}"), SHORT, || {
        let r = b.delete_object(key).map_err(|e| e.to_string())?;
        if r.status_code() / 100 == 2 || r.status_code() == 404 { Ok(()) } else { Err(format!("HTTP {}", r.status_code())) }
    })
}

/// Fill `buf` from `r` until full or EOF; returns bytes read.
pub fn fill(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
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

/// Stream `r` to `key` with `concurrency` parallel part uploads. Returns bytes uploaded.
pub fn upload_parallel(b: &Bucket, key: &str, r: &mut impl Read, concurrency: usize) -> Result<u64, String> {
    let concurrency = concurrency.clamp(1, 32);
    let mut first = vec![0u8; PART];
    let n = fill(r, &mut first).map_err(|e| format!("read input: {e}"))?;
    if n < PART {
        retry(&format!("upload {key}"), LONG, || put(b, key, &first[..n]))?;
        return Ok(n as u64);
    }
    let id = retry(&format!("start upload {key}"), LONG, || b.initiate_multipart_upload(key, CT).map_err(|e| e.to_string()))?
        .upload_id;
    let failed = Arc::new(AtomicBool::new(false));
    let err: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let parts = Arc::new(Mutex::new(Vec::new()));
    let (tx, rx) = mpsc::sync_channel::<(u32, Vec<u8>)>(concurrency);
    let rx = Arc::new(Mutex::new(rx));
    let mut total = n as u64;
    std::thread::scope(|s| {
        for _ in 0..concurrency {
            let (rx, failed, err, parts, id) = (rx.clone(), failed.clone(), err.clone(), parts.clone(), id.clone());
            s.spawn(move || loop {
                let job = rx.lock().unwrap().recv();
                let Ok((num, data)) = job else { return };
                if failed.load(Ordering::Relaxed) {
                    continue; // drain
                }
                match retry(&format!("upload {key} part {num}"), LONG, || {
                    b.put_multipart_chunk(&data, key, num, &id, CT).map_err(|e| e.to_string())
                }) {
                    Ok(p) => parts.lock().unwrap().push(p),
                    Err(e) => {
                        failed.store(true, Ordering::Relaxed);
                        err.lock().unwrap().get_or_insert(e);
                    }
                }
            });
        }
        let mut num = 1u32;
        let mut cur = first;
        let mut len = n;
        loop {
            if failed.load(Ordering::Relaxed) {
                break;
            }
            cur.truncate(len);
            if tx.send((num, cur)).is_err() {
                break;
            }
            let mut next = vec![0u8; PART];
            match fill(r, &mut next) {
                Ok(0) => break,
                Ok(k) => {
                    total += k as u64;
                    len = k;
                    cur = next;
                    num += 1;
                }
                Err(e) => {
                    failed.store(true, Ordering::Relaxed);
                    err.lock().unwrap().get_or_insert(format!("read input: {e}"));
                    break;
                }
            }
        }
        drop(tx);
    });
    if let Some(e) = err.lock().unwrap().take() {
        let _ = b.abort_upload(key, &id);
        return Err(e);
    }
    let mut parts = std::mem::take(&mut *parts.lock().unwrap());
    parts.sort_by_key(|p| p.part_number);
    let done = retry(&format!("finish upload {key}"), LONG, || {
        let resp = b.complete_multipart_upload(key, &id, parts.clone()).map_err(|e| e.to_string())?;
        if resp.status_code() / 100 == 2 { Ok(()) } else { Err(format!("HTTP {}", resp.status_code())) }
    });
    if let Err(e) = done {
        let _ = b.abort_upload(key, &id);
        return Err(e);
    }
    Ok(total)
}

/// Download `key` (`size` bytes) into `w` with `concurrency` parallel range requests, written in order.
/// At most `2 x concurrency` chunks are buffered. Network errors retry the range; writer errors stop at once.
pub fn download_parallel(b: &Bucket, key: &str, size: u64, w: &mut impl Write, concurrency: usize) -> Result<u64, String> {
    let concurrency = concurrency.clamp(1, 32);
    let chunk = PART as u64;
    let chunks = size.div_ceil(chunk) as usize;
    if chunks == 0 {
        return Ok(0);
    }
    let window = concurrency * 2;
    let next = AtomicUsize::new(0);
    // (chunks fetched and not yet written, next chunk the writer needs, first error)
    type Window = (BTreeMap<usize, Vec<u8>>, usize, Option<String>);
    let state: Mutex<Window> = Mutex::new((BTreeMap::new(), 0, None));
    let cv = Condvar::new();
    let mut written = 0u64;
    let mut werr: Option<String> = None;
    std::thread::scope(|s| {
        for _ in 0..concurrency.min(chunks) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                if i >= chunks {
                    return;
                }
                {
                    // wait until the writer is within the window (bounded memory)
                    let mut g = state.lock().unwrap();
                    while i >= g.1 + window && g.2.is_none() {
                        g = cv.wait(g).unwrap();
                    }
                    if g.2.is_some() {
                        return;
                    }
                }
                let from = i as u64 * chunk;
                let to = (from + chunk).min(size) - 1;
                let r = retry(&format!("download {key} bytes {from}-{to}"), LONG, || {
                    let resp = b.get_object_range(key, from, Some(to)).map_err(|e| e.to_string())?;
                    if resp.status_code() / 100 != 2 {
                        return Err(format!("HTTP {}", resp.status_code()));
                    }
                    let v = resp.to_vec();
                    if v.len() as u64 != to - from + 1 {
                        return Err(format!("short range: {} of {} bytes", v.len(), to - from + 1));
                    }
                    Ok(v)
                });
                let mut g = state.lock().unwrap();
                match r {
                    Ok(v) => {
                        g.0.insert(i, v);
                    }
                    Err(e) => {
                        g.2.get_or_insert(e);
                    }
                }
                cv.notify_all();
            });
        }
        // writer (this thread): take chunks in order
        let mut idx = 0usize;
        while idx < chunks {
            let data = {
                let mut g = state.lock().unwrap();
                loop {
                    if let Some(d) = g.0.remove(&idx) {
                        break Some(d);
                    }
                    if g.2.is_some() {
                        break None;
                    }
                    g = cv.wait(g).unwrap();
                }
            };
            let Some(d) = data else { break };
            if let Err(e) = w.write_all(&d) {
                werr = Some(format!("write: {e}"));
                state.lock().unwrap().2.get_or_insert("writer stopped".into());
                cv.notify_all();
                break;
            }
            written += d.len() as u64;
            idx += 1;
            state.lock().unwrap().1 = idx;
            cv.notify_all();
        }
    });
    if let Some(e) = werr {
        return Err(e);
    }
    if let Some(e) = state.into_inner().unwrap().2 {
        return Err(e);
    }
    Ok(written)
}

#[cfg(test)]
mod t {
    use super::*;

    /// Round trip of a large stream through the parallel multipart upload and the ranged parallel download, against
    /// a real S3: PGBX_TEST_S3_ENDPOINT, PGBX_TEST_S3_BUCKET, PGBX_TEST_S3_CREDS (credentials file), PGBX_TEST_MB (1500).
    ///   cargo test --release -- --ignored --nocapture s3_roundtrip_large
    #[test]
    #[ignore]
    fn s3_roundtrip_large() {
        use sha2::{Digest, Sha256};
        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("set {k}"));
        let c = S3Conf {
            endpoint: env("PGBX_TEST_S3_ENDPOINT"),
            bucket: env("PGBX_TEST_S3_BUCKET"),
            region: "us-east-1".into(),
            credentials_file: env("PGBX_TEST_S3_CREDS"),
        };
        let mb: usize = std::env::var("PGBX_TEST_MB").ok().and_then(|v| v.parse().ok()).unwrap_or(1500);
        let b = bucket(&c).unwrap();
        // incompressible pseudo-random bytes (xorshift), generated as they are read: no buffer of the whole stream
        struct Gen(u64, usize);
        impl Read for Gen {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = buf.len().min(self.1);
                for x in buf[..n].chunks_mut(8) {
                    self.0 ^= self.0 << 13;
                    self.0 ^= self.0 >> 7;
                    self.0 ^= self.0 << 17;
                    let l = x.len();
                    x.copy_from_slice(&self.0.to_le_bytes()[..l]);
                }
                self.1 -= n;
                Ok(n)
            }
        }
        struct H<R: Read>(R, Sha256);
        impl<R: Read> Read for H<R> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let k = self.0.read(buf)?;
                self.1.update(&buf[..k]);
                Ok(k)
            }
        }
        struct W(Sha256, u64);
        impl Write for W {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.update(b);
                self.1 += b.len() as u64;
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let key = format!("roundtrip-{}.bin", std::process::id());
        for conc in [4usize, 8] {
            let mut src = H(Gen(0x9E37_79B9_7F4A_7C15, mb << 20), Sha256::new());
            let t = std::time::Instant::now();
            let n = upload_parallel(&b, &key, &mut src, conc).unwrap();
            let up = t.elapsed().as_secs_f64();
            assert_eq!(n as usize, mb << 20);
            let mut w = W(Sha256::new(), 0);
            let t = std::time::Instant::now();
            download_parallel(&b, &key, n, &mut w, conc).unwrap();
            let down = t.elapsed().as_secs_f64();
            assert_eq!(w.0.finalize(), src.1.finalize(), "content differs");
            println!(
                "{mb} MB ({} parts), concurrency {conc}: upload {:.0} MB/s, download {:.0} MB/s",
                n.div_ceil(PART as u64),
                mb as f64 / up,
                mb as f64 / down
            );
            delete(&b, &key).unwrap();
        }
    }
}
