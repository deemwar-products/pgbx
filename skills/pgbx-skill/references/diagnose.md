# Diagnose — Postgres down, disk full, pg_wal growing

pgbx only diagnoses. Steps carry a tier: `readonly` / `safe` / `guarded` / `destructive` (needs human approval).

### DIAG-R-1: Why is Postgres down?

**When to use:** "postgres is down", "won't start", "disk full", "OOM", "postgres crashed", "pgbx diagnose".

**Command:**
```bash
pgbx diagnose --json                          # data dir from $PGDATA or the usual paths
pgbx diagnose --json --pgdata /var/lib/postgresql/16/main --log /tmp/pg.log   # docker: docker logs db > /tmp/pg.log
```

**Expected response:** `{"postgres":"up|down|starting|shutting_down|recovering","probable_cause":"oom_kill|disk_full|wal_backlog|stale_pid|crash|corruption|permissions|config_error|too_many_connections|unknown","evidence":[{"source","line"}],"steps":[{"tier","why","command","needs_human_approval"}],"facts":{"space":[...],"ready_wal":N}}`.
`pgbx doctor --json` embeds the same object as `diagnosis` when Postgres is unreachable.

**Common errors:** `unknown` with no evidence → logs not readable: pass `--log` (and run as root/postgres for the
kernel log). `pgdata: null` → pass `--pgdata`.

**User-visible formatting:** "Postgres is <postgres>: <probable_cause>." + the evidence lines + numbered steps, each tagged with its tier.

### DIAG-R-2: pg_wal growing (Postgres up)

**When to use:** "pg_wal is growing", "wal backlog", replication slot questions.

**Command:**
```bash
pgbx doctor --json
psql -XAtq -d postgres -c "SELECT row_to_json(d) FROM pgbx.doctor() d WHERE name IN ('replication_slots','archive_mode')"
```

**Expected response:** `replication_slots` (inactive slots pinning WAL, `max_slot_wal_keep_size`, `max_wal_size`,
`wal_keep_size`); `archive_mode` (info: pgbx needs no WAL archiving; a failing archive_command that something
else set also makes pg_wal grow).

**Common errors:** an inactive slot pins WAL → dropping it is destructive and needs human approval of that slot.
A third-party archive_command failing → fix that tool; never delete files in pg_wal.

**User-visible formatting:** only non-ok rows, each with its fix verbatim.

### DIAG-R-4: Corruption suspected

**When to use:** `probable_cause = corruption` ("invalid page in block", "could not open file", checksum failures).

**Command:**
```bash
pgbx db-restore --from-s3 --db myapp --into myapp_check --s3-endpoint https://hel1.your-objectstorage.com --s3-bucket my-backups --s3-region hel1 --server-name db1 --credentials-file ./s3.credentials --host /tmp/other-server --json
```
Restore the newest dump into a NEW database (ideally on another, healthy server) and compare.

**Expected response:** `{restored_into, key, bytes}`. Repairing in place (pg_resetwal, zero_damaged_pages,
deleting files) is never offered.

**Common errors:** `already exists` → pick a new name. Swapping it in for the live database is destructive (approval).

**User-visible formatting:** "Corruption suspected in <evidence>. A copy of <db> is restored as <new>; nothing live was changed."
