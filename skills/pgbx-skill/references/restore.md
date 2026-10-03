# Restore — recipes

Every restore goes into a NEW database = safe mutation. Swapping it in for the live database is
**destructive** (RST-R-2, explicit approval). Whole-server restore to a moment is the optional point-in-time restore (`pitr.md`, 0.6.0+).

**User-visible formatting (family default):** "Restored <what> as of <time> into <where>; <what was untouched>."

---

### RST-R-1: Restore one database into a new database

**When to use:** "restore the db", "get yesterday's rows back", bad migration on one database.

**Command:**
```bash
pgbx db-restore --db myapp --into myapp_restored --time '2026-10-01T09:00:00Z' --json
```
Omit `--time` for the newest backup.
Fallback (SQL, inside `myapp`): `psql -XAtq -d myapp -c "SELECT pgbx.restore(into_db => 'myapp_restored', at => '2026-10-01 09:00+00')"` → history id; wait with BKP-R-3.

**Expected response:** `{id, state: "done", into_db, at}`. Uses the newest dump at or before `--time`.

**Common errors:** target database already exists → choose a new name; no backup before that time → STAT-R-2.

**User-visible formatting:** family default; live `myapp` untouched.

### RST-R-2: Compare and swap (destructive part)

**When to use:** after RST-R-1, the user wants the restored copy to become live.

**Command:**
```bash
psql -XAtq -d postgres -c "SELECT json_agg(d) FROM (SELECT datname, pg_database_size(datname) FROM pg_database WHERE datname IN ('myapp','myapp_restored')) d"
```
Renaming/dropping the live database is destructive: ask, quoting "live `myapp` (<size>) will be replaced
by `myapp_restored` as of <time>; writes since then are lost", then follow the human's instruction.

**Expected response:** sizes of both databases.

**Common errors:** active connections block `ALTER DATABASE … RENAME`; report, don't terminate sessions unasked.

**User-visible formatting:** the quote above, then wait.

### RST-R-3: List backups in S3 (server gone, no extension needed)

**When to use:** the old server is lost; find which dumps exist for a database (step-03).

**Command:**
```bash
pgbx backups --from-s3 --db myapp --s3-endpoint https://hel1.your-objectstorage.com --s3-bucket my-backups --s3-region hel1 --server-name db1 --credentials-file ./s3.credentials --json
```
`--db` is the database's folder (its `path`, default its name). The credentials file has
`access_key_id=` / `secret_access_key=` lines; never read or print it.

**Expected response:** `{prefix, backups: [{key, taken_at, bytes}]}`, newest first.

**Common errors:** empty list → wrong `--server-name` or `--db` folder; access denied → credentials/bucket.

**User-visible formatting:** "<n> backups of myapp in S3, newest <taken_at> (<size>)."

### RST-R-4: Restore a database from S3 onto a new server

**When to use:** step-03; the target server may not have pgbx at all.

**Command:**
```bash
pgbx db-restore --from-s3 --db myapp --into myapp_restored --time '2026-10-01T09:00:00Z' --s3-endpoint https://hel1.your-objectstorage.com --s3-bucket my-backups --s3-region hel1 --server-name db1 --credentials-file ./s3.credentials --host /var/run/postgresql --user postgres --json
```
Omit `--time` for the newest dump; `--backup <key>` picks one exactly. Needs `pg_restore` on PATH.

**Expected response:** `{restored_into, key, bytes}`; newest dump at or before `--time`, streamed into
`pg_restore --no-owner` with resume on network drops.

**Common errors:** `already exists` → choose a new name; `no backup … at or before` → RST-R-3 for valid times;
`--time` without a UTC offset → add `Z`/`+00`.

**User-visible formatting:** "Restored myapp as of <taken_at> into myapp_restored on <server>; nothing else touched."

### RST-R-5: Restore with the roles it needs (owners and grants kept)

**When to use:** "restore with roles", "role does not exist after restore", "restore onto a new server with users".

**Command:**
```bash
pgbx db-restore --db myapp --into myapp_restored --with-roles --wait --json          # same server
pgbx db-restore --from-s3 --db myapp --into myapp --with-roles [--roles all] \
  [--key-file /path/backup.key] <s3 flags> --json                                    # new server
```
Fallback (SQL): `psql -XAtq -d myapp -c "SELECT pgbx.restore('myapp_restored', with_roles => true)"`
Missing roles are created from the backup's roles file; existing roles are never changed. `--roles referenced`
(default) = only roles this database uses; `all` = every role of the old server (ask first on a shared server).
Passwords are not in the file unless `pgbx.backup_role_passwords = on`: tell the user which roles need one.
Encrypted backups (`pgbx.encryption_key_file`): same server needs nothing; `--from-s3` needs `--key-file`.

**Expected response:** `roles: {"created":[..],"existing":[..],"out_of_scope":[..],"skipped":[..],"failed":[..]}`.

**Common errors:** `roles file ... (backups taken before pgbx 0.6 have none ...)` → restore without `--with-roles`;
`this backup is encrypted: give the key` → add `--key-file`; `wrong key, or the file was modified` → other key file.

**User-visible formatting:** "Restored into <db>; created roles <list> (no passwords); left existing <list> unchanged."
