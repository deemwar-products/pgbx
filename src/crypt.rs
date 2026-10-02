//! Optional client-side encryption of dumps: AES-256-GCM over a stream, in large frames.
//! Shared by the extension (encrypt on backup, decrypt on restore/verify) and the CLI (`--key-file`,
//! `pgbx decrypt`); it has no pgrx dependency so both crates compile it (`#[path]`).
//!
//! Format (version 1):
//!   header  16 bytes: "PGBXENC1" | version u8 = 1 | frame size log2 u8 | nonce prefix 6 bytes (random per file)
//!   frames  each: u32 BE ciphertext length (high bit = final frame) | ciphertext | 16-byte GCM tag
//! Nonce of frame i = prefix(6) | i as u32 BE (4) | 0 | final(0/1) (12 bytes). The whole header is the AAD of
//! every frame, so it cannot be altered. A stream must end with exactly one final frame: a cut-off file fails
//! (truncation), a reordered or dropped frame fails (counter in the nonce), a flipped bit fails (tag), a wrong
//! key fails on the first frame. Memory: one frame per stream, whatever the dump size; no temp files.
//!
//! The key is 32 random bytes in a file (base64 or hex, one line), readable by its owner only (chmod 600):
//!   head -c 32 /dev/urandom | base64 > /etc/pgbx/backup.key

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use std::io::{self, Read, Write};

pub const MAGIC: &[u8; 8] = b"PGBXENC1";
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 16;
pub const TAG_LEN: usize = 16;
/// 4 MiB frames: large enough that per-frame overhead (20 bytes, one syscall-ish copy) is noise.
pub const FRAME_LOG2: u8 = 22;
const FINAL: u32 = 1 << 31;
const MAX_FRAME_LOG2: u8 = 26; // refuse headers that ask for frames > 64 MiB (memory bound on decrypt)

pub struct Key(LessSafeKey);

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

impl Key {
    pub fn from_bytes(b: &[u8]) -> Result<Key, String> {
        if b.len() != 32 {
            return Err(format!("encryption key must be 32 bytes, got {}", b.len()));
        }
        Ok(Key(LessSafeKey::new(UnboundKey::new(&AES_256_GCM, b).map_err(|_| "bad key")?)))
    }

    /// Key file text: 32 bytes as base64 (44 chars) or hex (64 chars). Never echoes the content in errors.
    pub fn parse(text: &str) -> Result<Key, String> {
        let t: String = text.split_whitespace().collect();
        if t.len() == 64 && t.chars().all(|c| c.is_ascii_hexdigit()) {
            let v: Vec<u8> = (0..32).map(|i| u8::from_str_radix(&t[2 * i..2 * i + 2], 16).unwrap()).collect();
            return Key::from_bytes(&v);
        }
        match b64(&t) {
            Some(v) if v.len() == 32 => Key::from_bytes(&v),
            _ => Err("key file must hold 32 random bytes as base64 or hex (head -c 32 /dev/urandom | base64)".into()),
        }
    }

    /// Read a key file; refuses one that group/others can read (unix).
    pub fn load(path: &str) -> Result<Key, String> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let m = std::fs::metadata(path).map_err(|e| format!("key file {path}: {e}"))?;
            if m.permissions().mode() & 0o077 != 0 {
                return Err(format!("key file {path} is readable by group/others; chmod 600 it"));
            }
        }
        let text = std::fs::read_to_string(path).map_err(|e| format!("key file {path}: {e}"))?;
        Key::parse(&text).map_err(|e| format!("key file {path}: {e}"))
    }
}

/// Minimal standard/url-safe base64 decoder (padding optional).
fn b64(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32)
    };
    let s = s.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for &c in s {
        acc = (acc << 6) | val(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

fn nonce(prefix: &[u8; 6], i: u32, fin: bool) -> Nonce {
    let mut n = [0u8; 12];
    n[..6].copy_from_slice(prefix);
    n[6..10].copy_from_slice(&i.to_be_bytes());
    n[11] = fin as u8;
    Nonce::assume_unique_for_key(n)
}

/// True when `head` starts like an encrypted pgbx stream.
pub fn is_encrypted(head: &[u8]) -> bool {
    head.len() >= 8 && &head[..8] == MAGIC
}

fn fill(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// `Read` adapter: plaintext in, encrypted stream out.
pub struct EncryptReader<'k, R: Read> {
    inner: R,
    key: &'k Key,
    header: [u8; HEADER_LEN],
    frame: usize,
    out: Vec<u8>, // [len u32][ciphertext][tag] of the current frame (or the header first)
    pos: usize,
    carry: Option<u8>, // one byte read ahead to know whether a full frame is the last one
    counter: u32,
    done: bool,
}

impl<'k, R: Read> EncryptReader<'k, R> {
    pub fn new(inner: R, key: &'k Key) -> Self {
        Self::with_frame_log2(inner, key, FRAME_LOG2)
    }

    pub fn with_frame_log2(inner: R, key: &'k Key, log2: u8) -> Self {
        let mut prefix = [0u8; 6];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut prefix).expect("system random");
        let mut header = [0u8; HEADER_LEN];
        header[..8].copy_from_slice(MAGIC);
        header[8] = VERSION;
        header[9] = log2;
        header[10..16].copy_from_slice(&prefix);
        let frame = 1usize << log2;
        let mut out = Vec::with_capacity(4 + frame + TAG_LEN);
        out.extend_from_slice(&header);
        EncryptReader { inner, key, header, frame, out, pos: 0, carry: None, counter: 0, done: false }
    }

    fn next_frame(&mut self) -> io::Result<()> {
        let f = self.frame;
        self.out.clear();
        self.out.resize(4 + f, 0);
        let mut n = 0;
        if let Some(c) = self.carry.take() {
            self.out[4] = c;
            n = 1;
        }
        n += fill(&mut self.inner, &mut self.out[4 + n..4 + f])?;
        let fin = if n < f {
            true
        } else {
            let mut one = [0u8; 1];
            match fill(&mut self.inner, &mut one)? {
                0 => true,
                _ => {
                    self.carry = Some(one[0]);
                    false
                }
            }
        };
        if self.counter == u32::MAX {
            return Err(bad("stream too long for one encrypted file"));
        }
        self.out.truncate(4 + n);
        let prefix: [u8; 6] = self.header[10..16].try_into().unwrap();
        let tag = self
            .key
            .0
            .seal_in_place_separate_tag(nonce(&prefix, self.counter, fin), Aad::from(&self.header), &mut self.out[4..])
            .map_err(|_| bad("encrypt failed"))?;
        self.out.extend_from_slice(tag.as_ref());
        let len = (n as u32) | if fin { FINAL } else { 0 };
        self.out[..4].copy_from_slice(&len.to_be_bytes());
        self.counter += 1;
        self.done = fin;
        self.pos = 0;
        Ok(())
    }
}

impl<R: Read> Read for EncryptReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos == self.out.len() {
            if self.done {
                return Ok(0);
            }
            self.next_frame()?;
        }
        let k = buf.len().min(self.out.len() - self.pos);
        buf[..k].copy_from_slice(&self.out[self.pos..self.pos + k]);
        self.pos += k;
        Ok(k)
    }
}

/// `Write` adapter: encrypted stream in, plaintext written to `inner`. With `passthrough_plain`, a stream that
/// does not start with the magic is forwarded untouched (unencrypted dumps keep working). Call `finish()` at
/// the end: it fails if the final frame never arrived (truncated file).
pub struct DecryptWriter<'k, W: Write> {
    inner: W,
    key: Option<&'k Key>,
    passthrough_plain: bool,
    buf: Vec<u8>,
    header: Option<[u8; HEADER_LEN]>,
    plain: bool,
    frame: usize,
    counter: u32,
    finished: bool,
    pub plaintext_bytes: u64,
    /// First decryption/destination error, kept so a caller that only sees "write failed" can report why.
    pub error: Option<String>,
}

impl<'k, W: Write> DecryptWriter<'k, W> {
    pub fn new(inner: W, key: Option<&'k Key>, passthrough_plain: bool) -> Self {
        DecryptWriter { inner, key, passthrough_plain, buf: Vec::new(), header: None, plain: false, frame: 0, counter: 0,
                        finished: false, plaintext_bytes: 0, error: None }
    }

    /// Was the stream encrypted? (None until the first 8 bytes arrived.)
    pub fn encrypted(&self) -> Option<bool> {
        if self.plain { Some(false) } else if self.header.is_some() { Some(true) } else { None }
    }

    fn process(&mut self) -> io::Result<()> {
        let mut off = 0;
        if self.header.is_none() && !self.plain {
            if self.buf.len() < 8 {
                return Ok(());
            }
            if !is_encrypted(&self.buf) {
                if !self.passthrough_plain {
                    return Err(bad("not an encrypted pgbx stream"));
                }
                self.plain = true;
                let b = std::mem::take(&mut self.buf);
                self.plaintext_bytes += b.len() as u64;
                return self.inner.write_all(&b);
            }
            if self.buf.len() < HEADER_LEN {
                return Ok(());
            }
            let h: [u8; HEADER_LEN] = self.buf[..HEADER_LEN].try_into().unwrap();
            if h[8] != VERSION {
                return Err(bad(format!("encrypted with format version {}; this pgbx reads version {VERSION}", h[8])));
            }
            if h[9] < 10 || h[9] > MAX_FRAME_LOG2 {
                return Err(bad("corrupted encryption header (frame size)"));
            }
            if self.key.is_none() {
                return Err(bad("this backup is encrypted: give the key (pgbx.encryption_key_file / --key-file)"));
            }
            self.frame = 1 << h[9];
            self.header = Some(h);
            off = HEADER_LEN;
        }
        let h = self.header.unwrap();
        let prefix: [u8; 6] = h[10..16].try_into().unwrap();
        let key = self.key.unwrap();
        loop {
            let rest = &self.buf[off..];
            if rest.len() < 4 {
                break;
            }
            if self.finished {
                return Err(bad("data after the final frame (corrupted or appended file)"));
            }
            let raw = u32::from_be_bytes(rest[..4].try_into().unwrap());
            let (n, fin) = ((raw & !FINAL) as usize, raw & FINAL != 0);
            if n > self.frame || (!fin && n != self.frame) {
                return Err(bad("corrupted frame length"));
            }
            if rest.len() < 4 + n + TAG_LEN {
                break;
            }
            let start = off + 4;
            let ctr = self.counter;
            let plain = key
                .0
                .open_in_place(nonce(&prefix, ctr, fin), Aad::from(&h), &mut self.buf[start..start + n + TAG_LEN])
                .map_err(|_| {
                    bad(if ctr == 0 {
                        "decryption failed: wrong key, or the file was modified"
                    } else {
                        "decryption failed: the file was modified or frames are out of order"
                    })
                })?;
            self.plaintext_bytes += plain.len() as u64;
            self.inner.write_all(plain)?;
            self.counter = self.counter.checked_add(1).ok_or_else(|| bad("too many frames"))?;
            self.finished = fin;
            off = start + n + TAG_LEN;
        }
        self.buf.drain(..off);
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<W> {
        if self.plain {
            return Ok(self.inner);
        }
        if self.header.is_none() {
            if self.buf.is_empty() && self.passthrough_plain {
                return Ok(self.inner); // empty input
            }
            if self.passthrough_plain && !self.buf.is_empty() && !is_encrypted(&self.buf) {
                let b = std::mem::take(&mut self.buf);
                self.inner.write_all(&b)?;
                return Ok(self.inner);
            }
            return Err(bad("truncated: encrypted stream ends inside its header"));
        }
        if !self.finished || !self.buf.is_empty() {
            return Err(bad("truncated: the encrypted backup ends before its final frame"));
        }
        self.inner.flush()?;
        Ok(self.inner)
    }
}

impl<W: Write> Write for DecryptWriter<'_, W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.plain {
            self.plaintext_bytes += data.len() as u64;
            self.inner.write_all(data)?;
            return Ok(data.len());
        }
        if self.buf.capacity() == 0 {
            self.buf.reserve(4 + (1 << FRAME_LOG2) + TAG_LEN + data.len());
        }
        self.buf.extend_from_slice(data);
        if let Err(e) = self.process() {
            if e.kind() == io::ErrorKind::InvalidData {
                self.error.get_or_insert_with(|| e.to_string());
            }
            return Err(e);
        }
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(b: u8) -> Key {
        Key::from_bytes(&[b; 32]).unwrap()
    }

    fn enc(k: &Key, data: &[u8], log2: u8) -> Vec<u8> {
        let mut out = vec![];
        EncryptReader::with_frame_log2(data, k, log2).read_to_end(&mut out).unwrap();
        out
    }

    fn dec(k: &Key, data: &[u8], chunk: usize) -> io::Result<Vec<u8>> {
        let mut w = DecryptWriter::new(Vec::new(), Some(k), false);
        for c in data.chunks(chunk.max(1)) {
            w.write_all(c)?;
        }
        w.finish()
    }

    fn sample(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 31 + i / 7) as u8).collect()
    }

    #[test]
    fn roundtrip_sizes_and_chunkings() {
        let k = key(7);
        for n in [0, 1, 1023, 1024, 1025, 4096, 10_000] {
            let d = sample(n);
            let e = enc(&k, &d, 10);
            assert!(is_encrypted(&e));
            assert_ne!(&e[HEADER_LEN..], &d[..], "ciphertext differs");
            for chunk in [1, 7, 1000, 1 << 20] {
                assert_eq!(dec(&k, &e, chunk).unwrap(), d, "n={n} chunk={chunk}");
            }
        }
        let d = sample(9 << 20);
        assert_eq!(dec(&k, &enc(&k, &d, FRAME_LOG2), 65536).unwrap(), d);
    }

    #[test]
    fn tamper_fails() {
        let k = key(1);
        let e = enc(&k, &sample(5000), 10);
        for pos in [3, 9, 12, HEADER_LEN + 2, HEADER_LEN + 4 + 10, e.len() - 1, e.len() / 2] {
            let mut t = e.clone();
            t[pos] ^= 0x01;
            assert!(dec(&k, &t, 4096).is_err(), "flip at {pos} must fail");
        }
    }

    #[test]
    fn truncation_fails() {
        let k = key(2);
        let e = enc(&k, &sample(5000), 10);
        let frame = 4 + 1024 + TAG_LEN;
        for cut in [5, HEADER_LEN, HEADER_LEN + 1, HEADER_LEN + frame, HEADER_LEN + 2 * frame, e.len() - 1] {
            assert!(dec(&k, &e[..cut], 4096).is_err(), "cut at {cut} must fail");
        }
        // dropping a whole middle frame (counter mismatch) and appending junk fail too
        let mut dropped = e[..HEADER_LEN + frame].to_vec();
        dropped.extend_from_slice(&e[HEADER_LEN + 2 * frame..]);
        assert!(dec(&k, &dropped, 4096).is_err());
        let mut appended = e.clone();
        appended.extend_from_slice(&e[HEADER_LEN..HEADER_LEN + frame]);
        assert!(dec(&k, &appended, 4096).is_err());
    }

    #[test]
    fn wrong_key_fails() {
        let e = enc(&key(3), b"hello", 10);
        let err = dec(&key(4), &e, 64).unwrap_err().to_string();
        assert!(err.contains("wrong key"), "{err}");
        let mut w = DecryptWriter::new(Vec::new(), None, true);
        assert!(w.write_all(&e).unwrap_err().to_string().contains("encrypted"));
    }

    #[test]
    fn plain_passthrough() {
        let mut w = DecryptWriter::new(Vec::new(), None, true);
        w.write_all(b"PGDMP").unwrap();
        w.write_all(b" rest of a custom dump").unwrap();
        assert_eq!(w.finish().unwrap(), b"PGDMP rest of a custom dump");
        let w = DecryptWriter::new(Vec::new(), None, true);
        assert_eq!(w.finish().unwrap(), b"");
        let k1 = key(1);
        let mut w = DecryptWriter::new(Vec::new(), Some(&k1), false);
        assert!(w.write_all(b"PGDMP........").is_err(), "strict mode refuses plaintext");
    }

    #[test]
    fn key_parsing() {
        let hex = "00".repeat(31) + "ff";
        assert!(Key::parse(&hex).is_ok());
        assert!(Key::parse("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=\n").is_ok());
        assert!(Key::parse("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8").is_ok());
        assert!(Key::parse("too short").is_err());
        assert!(Key::parse(&"ab".repeat(16)).is_err(), "16-byte hex is not a 32-byte key");
        let e = Key::parse("correct horse battery staple").err().unwrap();
        assert!(!e.contains("horse"), "never echo key material");
    }

    #[test]
    fn key_file_must_be_private() {
        use std::os::unix::fs::PermissionsExt;
        let p = std::env::temp_dir().join(format!("pgbx-key-test-{}", std::process::id()));
        std::fs::write(&p, "00".repeat(32)).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Key::load(p.to_str().unwrap()).err().unwrap().contains("chmod 600"));
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(Key::load(p.to_str().unwrap()).is_ok());
        let _ = std::fs::remove_file(p);
    }

    /// cargo test --release -- --ignored --nocapture crypt_throughput
    #[test]
    #[ignore]
    fn crypt_throughput() {
        let k = key(9);
        let d = sample(512 << 20);
        let t = std::time::Instant::now();
        let mut sink = Counting(0);
        io::copy(&mut EncryptReader::new(&d[..], &k), &mut sink).unwrap();
        let es = t.elapsed().as_secs_f64();
        let e = enc(&k, &d, FRAME_LOG2);
        let t = std::time::Instant::now();
        let mut w = DecryptWriter::new(Counting(0), Some(&k), false);
        for c in e.chunks(1 << 16) {
            w.write_all(c).unwrap();
        }
        assert_eq!(w.finish().unwrap().0, d.len() as u64);
        let ds = t.elapsed().as_secs_f64();
        let mb = d.len() as f64 / 1e6;
        println!("encrypt {:.0} MB/s, decrypt {:.0} MB/s (single core, {:.0} MB)", mb / es, mb / ds, mb);
    }

    struct Counting(u64);
    impl Write for Counting {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0 += b.len() as u64;
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
