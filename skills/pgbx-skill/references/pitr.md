# Point-in-time restore — recipes

Optional, whole server, off by default (`pgbx.pitr`). WAL is archived by `pgbx wal-push`, base backups run on
`pgbx.pitr_schedule` as jobs of the server-wide queue (kind `base_backup`, admin database). Per-database dumps
stay the default product. Restores go into a NEW directory you start on
another port (safe mutation); replacing a stopped server's data directory in place is **destructive**.

**User-visible formatting (family default):** "PITR <state>: restorable from <restorable_from> until <wal_archived_until>; <n> open gap(s)."

---

### PITR-R-1: Point-in-time restore status

**When to use:** "can we restore to any point", "pitr status", "what is the restore window".

**Command:**
```bash
pgbx pitr status --json
```
Fallback (admin database): `psql -XAtq -d postgres -c "SELECT row_to_json(s) FROM pgbx.pitr_status() s"`.

**Expected response:** `{pitr: {enabled, state, last_base_backup_at, restorable_from, wal_archived_until, open_gaps, gaps, backlog_segments, last_error}}`.

**Common errors:** `state = off` → PITR not enabled (PITR-R-2); `gap: …` → WAL was dropped under `pgbx.wal_queue_max`, no restore inside the gap until the healing base backup finishes.

**User-visible formatting:** family default.

### PITR-R-2: Enable point-in-time restore

**When to use:** "enable pitr", "turn on point in time recovery".

**Command:**
```bash
pgbx setup pitr --json            # shows the plan (guarded)
pgbx setup pitr --yes --json      # writes archive_mode=on, archive_command='<pgbx> wal-push %p', pgbx.pitr=on
```

**Expected response:** `{ok: true, restart_needed: true|false, next}`. archive_mode needs ONE Postgres restart; ask before restarting.

**Common errors:** "archive_command is already set to something pgbx did not write" → another tool archives WAL; never override it, report to the user.

**User-visible formatting:** "PITR enabled; restart Postgres once (<command>) and the first base backup starts within a minute."

### PITR-R-3: Restore the whole server to a moment (copy)

**When to use:** "restore the server to 09:00", "point in time restore", "undo everything after T".

**Command:**
```bash
pgbx pitr restore --time '2026-10-01 09:00:00+00' --target /srv/pg-restored --json \
  --conf /var/lib/postgresql/pgbx/pgbx-wal.conf
# server down / new host: --s3-endpoint U --s3-bucket B --server-name S --credentials-file F [--system-id N]
```
`--time latest` replays all archived WAL. pgbx never starts Postgres: run the printed `start_command` (another port).

**Expected response:** `{ok, base_backup, data_directory, mode, start_command}`; the copy has archive_mode=off, pgbx.pitr=off.

**Common errors:** "inside WAL gap" → pick a time outside the gap; "not empty" → choose an empty directory; in place over the real server needs `--yes-replace-whole-server` with the server STOPPED — destructive, explicit approval required (the old directory is moved aside, not deleted).

**User-visible formatting:** "Restored the whole server as of <time> into <dir> (from base backup <label>); start it with: <start_command>."

### PITR-R-4: Take a base backup now, or stop one

**When to use:** "take a base backup now", "the base backup is stuck", "cancel the base backup".

**Command:**
```bash
pgbx pitr backup-now --wait --json            # superuser; queued in the server-wide queue
pgbx jobs --json                              # shows it running or waiting, and why
pgbx jobs cancel <job_id> --yes --json        # guarded: stops its child; no half backup is ever kept
```

**Expected response:** `{ok, job_id, job: {state: "done", params: {label, start_time, stop_time, ...}}}`.

**Common errors:** "needs a superuser" → ask the human; "point-in-time restore is off" → PITR-R-2.

**User-visible formatting:** "Base backup <label> done in <s> s; restorable from <restorable_from>."
