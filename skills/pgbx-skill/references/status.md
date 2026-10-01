# Status — recipes

Tier: read-only (no approval). Commands require pgbx v0.3; `Fallback (SQL):` lines work today.

**User-visible formatting (family default):** one line per database:
"<db>: <state>, last backup <age> ago (<size>), next <time>, <n> kept, restore test <ok|FAILED|never>."

---

### STAT-R-1: Is it backed up?

**When to use:** "is postgres backed up", "backup status", "last backup".

**Command:**
```bash
pgbx status --json
```
Fallback (SQL): `psql -XAtq -d myapp -c "SELECT row_to_json(s) FROM pgbx.status() s"`
Server-wide: `psql -XAtq -d postgres -c "SELECT json_agg(o) FROM pgbx.overview() o"`

**Expected response:** per database: `state, schedule, next_backup_at, last_backup_at, last_backup_age,
last_backup_size, last_backup_key, backups_kept, retention, data_scope, verify_schedule, last_verify_result,
last_error, paused_reason, queued_jobs, location`.

| state | meaning | action |
|---|---|---|
| `active` | last backup ok | none; check `last_backup_age` vs schedule and `last_verify_result` |
| `running` | job in progress | wait, re-check in ~10 s |
| `waiting for first backup` | new database, nothing done yet | normal for minutes; if it lasts, check `last_error`; BKP-R-1 is safe |
| `failing` | newest failure after newest success | quote `last_error`, run STAT-R-5, report |
| `paused` | automatic backups stopped | manual still work; tell the human if > 1 h; POL-R-4 resume is safe if stale |
| `worker error` (overview) | worker can't reach that db | quote `last_error` |

**Common errors:**
- "… run in database …" (admin database) → `overview()` only in the admin database (`postgres`).
- "permission denied for schema pgbx" → caller needs `pgbx_viewer`.

**User-visible formatting:** family default; flag `data_scope` ≠ `all tables, all rows`.

### STAT-R-2: List backups

**When to use:** "list backups", "which backups do we have", picking a restore time.

**Command:**
```bash
pgbx list --json
```
Fallback (SQL): `psql -XAtq -d myapp -c "SELECT json_agg(b) FROM (SELECT id, taken_at, bytes, trigger, s3_key FROM pgbx.backups LIMIT 20) b"`

**Expected response:** newest first: `id, taken_at, size/bytes, trigger (manual|schedule|first|migration), s3_key`.

**Common errors:** empty list → `waiting for first backup`; see STAT-R-1.

**User-visible formatting:** numbered list: id, taken_at (local), size, trigger. Max 20.

### STAT-R-3: Job progress

**When to use:** after queuing any job, "is it done yet".

**Command:**
```bash
pgbx logs --json
```
Fallback (SQL): `psql -XAtq -d myapp -c "SELECT row_to_json(h) FROM (SELECT id, kind, state, s3_key, bytes, error, finished FROM pgbx.history WHERE id = 1234) h"`

**Expected response:** `state` one of `queued | running | done | failed | expired`; `error` set on failure.

**Common errors:** stuck `queued` → worker not running (`shared_preload_libraries`).

**User-visible formatting:** "Job #<id> <kind>: <state>" + error verbatim when failed.

### STAT-R-5: Doctor and logs

**When to use:** "backup failing", "why did the backup fail", "pgbx doctor".

**Command:**
```bash
pgbx doctor --json
pgbx logs --json
```
Fallback (SQL): `psql -XAtq -d postgres -c "SELECT row_to_json(d) FROM pgbx.doctor() d"` (pgbx v0.3);
`psql -XAtq -d myapp -c "SELECT json_agg(h) FROM (SELECT id, kind, state, error, finished FROM pgbx.history WHERE state = 'failed' ORDER BY id DESC LIMIT 5) h"`

**Expected response:** doctor checks with ok/warn/fail + fix; logs = recent jobs with errors.

**Common errors:** Postgres down → SQL impossible; `pgbx doctor` still works.

**User-visible formatting:** only the non-ok rows, each with its fix verbatim.

### STAT-R-6: Audit UI and timeline

**When to use:** "who did what", "who paused backups", "audit trail", "backup dashboard", "pgbx ui".

**Command:**
```bash
# a human-facing, read-only web UI (only GET; every connection SET default_transaction_read_only = on)
pgbx ui --user pgbx_ui --strict            # http://127.0.0.1:8432/ ; --listen IP:PORT to change
# the same data as JSON while it runs (agents: prefer this over scraping the page)
curl -s http://127.0.0.1:8432/api/timeline?days=30   # also /api/overview /api/db/<name> /api/health
```
Viewer login (ask the human first): `CREATE ROLE pgbx_ui LOGIN PASSWORD '...' IN ROLE pgbx_viewer`.
Fallback (SQL, per database): `psql -XAtq -d myapp -c "SELECT json_agg(h) FROM (SELECT id, kind, state, who, trigger, coalesce(finished, requested_at) AS at, error FROM pgbx.history ORDER BY id DESC LIMIT 50) h"`

**Expected response:** startup `{"ok":true,"url":..,"role":..,"warnings":[]}`; timeline rows `{database, kind, state, who, trigger, at, s3_key, error, params}`
newest first (kinds include backup, restore, verify, config, pause, resume, scope, download_url).

**Common errors:** `refusing (--strict): role ... is a SUPERUSER` / `can execute pgbx.backup_now()` → use a
`pgbx_viewer` login; a non-loopback `--listen` prints a warning — exposing it is the human's decision. History
older than `pgbx.audit_days` (default 30) is pruned except kept backups, open gaps and the newest row of each kind.

**User-visible formatting:** the URL, then one line per relevant row: "<at> <database> <kind> <state> by <who>".
