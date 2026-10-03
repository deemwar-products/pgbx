# Security — recipes

Client-side encryption of per-database dumps and their roles files (point-in-time base backups and WAL are not
encrypted by pgbx: S3 server-side encryption covers them). Never print, copy into chat, or move a key file; creating or changing the key
is the human's call.

**User-visible formatting (family default):** encrypted yes/no, which key file is configured (path only).

---

### SEC-R-1: Is encryption on?

**When to use:** "are backups encrypted", "is the dump encrypted".

**Command:**
```bash
pgbx metrics | grep pgbx_last_backup_encrypted
psql -XAtq -d postgres -c "SHOW pgbx.encryption_key_file"
```

**Expected response:** `pgbx_last_backup_encrypted{database="myapp"} 1`; a path (or empty = off).

**Common errors:** empty setting = encryption off (default).

**User-visible formatting:** "<db>: newest backup encrypted <yes/no>; key file <path>."

### SEC-R-2: Turn encryption on

**When to use:** "encrypt the backups", "client-side encryption".

**Call sequence:**
1. Ask the human to create the key and store a copy OFF the server (lost key = unrestorable backups):
   `head -c 32 /dev/urandom | base64 > /etc/pgbx/backup.key; chown postgres /etc/pgbx/backup.key; chmod 600 /etc/pgbx/backup.key`
2. `ALTER SYSTEM SET pgbx.encryption_key_file = '/etc/pgbx/backup.key'; SELECT pg_reload_conf();` (human approval).
3. `pgbx now --db myapp --wait --json` then SEC-R-1.

**Expected response:** the new backup's history `params.encrypted = true`.

**Common errors:** `readable by group/others; chmod 600 it`; `must hold 32 random bytes as base64 or hex`.

**User-visible formatting:** "Encryption on from backup <id>; older backups stay unencrypted until they expire."

### SEC-R-3: Decrypt a downloaded dump

**When to use:** "decrypt the dump", "download link gives garbage", "pg_restore: input file does not appear to be a valid archive".

**Command:**
```bash
curl -s "$URL" | pgbx decrypt --key-file /path/backup.key | pg_restore -d scratch_db
pgbx decrypt --key-file /path/backup.key --in shop.enc --out shop.dump
```

**Expected response:** `pgbx decrypt: N bytes decrypted` on stderr; plaintext dump on stdout / `--out`.

**Common errors:** `wrong key, or the file was modified`; `truncated` (download cut off: fetch again).

**User-visible formatting:** "Decrypted <N> bytes into <file / database>."
