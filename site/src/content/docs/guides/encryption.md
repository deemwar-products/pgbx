---
title: Encrypting backups
description: Client-side AES-256-GCM encryption of every dump, streamed, with one key file.
sidebar: { order: 13 }
---

Encryption is **off by default**. When `pgbx.encryption_key_file` is set, every per-database dump and its
[roles file](../roles/) are encrypted **before** they leave the server: S3 (and anyone holding a download link) only
ever sees ciphertext.

```sh
# 32 random bytes, base64 (hex also accepted); readable by postgres only
head -c 32 /dev/urandom | base64 | sudo tee /etc/pgbx/backup.key >/dev/null
sudo chown postgres /etc/pgbx/backup.key && sudo chmod 600 /etc/pgbx/backup.key
```

```ini
# postgresql.conf, then SELECT pg_reload_conf();
pgbx.encryption_key_file = '/etc/pgbx/backup.key'
```

**Keep a copy of the key somewhere other than the server** (a password manager, a vault). Without it the
backups cannot be restored: there is no recovery. A key file that group or others can read is refused (the backup
fails before anything is uploaded), and the key is never printed in an error.

## How it works

- AES-256-GCM over the `pg_dump` stream in 4 MiB frames: header `PGBXENC1` + version + random nonce prefix,
  then length-prefixed frames, each with its own 16-byte tag. Memory stays at one frame; no temp files.
- Tampering, a truncated file, frames out of order or the wrong key all **fail** the restore; nothing is
  written from an unauthenticated frame.
- Restores, restore tests and `--from-s3` restores detect encrypted dumps by their header and decrypt in the
  stream; older unencrypted dumps keep working (mixed folders are fine).
- Each backup's `history.params.encrypted` and `pgbx metrics` (`pgbx_last_backup_encrypted`) say whether the newest
  backup is encrypted.

- Measured cost (2026-10-02, Apple M5 Pro, local RustFS; `cargo test --release -- --ignored crypt_throughput` and
  `tests/encryption_bench.sh`): one core encrypts at 6.6–7.1 GB/s and decrypts at 7.3–7.9 GB/s (macOS and the
  Linux arm64 container alike), far above what `pg_dump`, compression and the upload deliver. End to end, a 1.22 GB
  database (372 MB zstd:3 dump) backed up in 7.2–8.4 s plain and 8.1–8.2 s encrypted, alternating, 3 runs each:
  medians 8.26 s and 8.10 s, so no measurable cost. One restore of each took 7.2 s (plain) and 8.8 s (encrypted).

## Restoring

```sh
# same server: nothing to do, the worker has the key
psql -d shop -c "SELECT pgbx.restore('shop_copy')"

# new server
pgbx db-restore --from-s3 --db shop --into shop --key-file /root/backup.key <s3 flags>
```

## Download links give ciphertext

`pgbx.download_url()` links serve the object as stored. Decrypt it with the same key:

```sh
curl -s "$url" | pgbx decrypt --key-file backup.key | pg_restore -d shop_copy
curl -s "$url" -o shop.enc && pgbx decrypt --key-file backup.key --in shop.enc --out shop.dump
```

## Changing the key

There is no rotation list: one key reads and writes. To change it, keep the old key file (to restore old
backups with `--key-file`), set the new one, and take a fresh backup (`pgbx now --wait`). Old backups age out
under your retention.

## What is not encrypted

[Point-in-time restore](../point-in-time-restore/) base backups and WAL are compressed and checksummed but not
encrypted by pgbx; use S3 server-side encryption for them.
