//! `pgbx decrypt --key-file F [--in FILE] [--out FILE]`: turn an encrypted dump (for example one fetched
//! with a download link, which serves the ciphertext as stored) back into a pg_restore-able dump. Streams:
//!   curl -s "$url" | pgbx decrypt --key-file /etc/pgbx/backup.key | pg_restore -d shop_copy

use crate::crypt::{DecryptWriter, Key};
use crate::{Ctx, Out};
use serde_json::json;
use std::fs::File;
use std::io::{self, BufWriter, Read, Write};

pub fn cmd(cx: &mut Ctx) -> Out {
    let kf = cx.a.get("key-file").ok_or("--key-file F is required (the pgbx.encryption_key_file of the server)")?;
    let key = Key::load(kf)?;
    let mut input: Box<dyn Read> = match cx.a.get("in") {
        Some(p) => Box::new(File::open(p).map_err(|e| format!("--in {p}: {e}"))?),
        None => Box::new(io::stdin().lock()),
    };
    let out_path = cx.a.get("out").map(String::from);
    let output: Box<dyn Write> = match &out_path {
        Some(p) => Box::new(File::create(p).map_err(|e| format!("--out {p}: {e}"))?),
        None => Box::new(io::stdout().lock()),
    };
    let mut w = DecryptWriter::new(BufWriter::with_capacity(1 << 20, output), Some(&key), false);
    io::copy(&mut input, &mut w).map_err(|e| w.error.clone().unwrap_or_else(|| e.to_string()))?;
    let n = w.plaintext_bytes;
    let mut inner = w.finish().map_err(|e| e.to_string())?;
    inner.flush().map_err(|e| e.to_string())?;
    if n == 0 && out_path.is_some() {
        return Err("nothing decrypted".into());
    }
    Ok(json!({"ok": true, "bytes": n, "out": out_path}))
}
