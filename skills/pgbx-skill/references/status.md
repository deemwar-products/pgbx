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

No extension on the server: `{ok: true, backups: "off", status: null, info: "pgbx extension not installed on this
server: backups are off; ..."}`. Say "Not backed up by pgbx: the extension is not installed on this server" and
give the optional `next_steps` line once. Other routes that only read (STAT-R-7, memory) still work.

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

**Expected response:** `state` one of `queued | running | done | failed | expired | cancelled`; `error` set on
failure. For where a job is in line and why it waits, use STAT-R-8 (`pgbx jobs`).

**Common errors:** stuck `queued` → `pgbx jobs` says why (a job slot, a running dump of the same database, the load
gate); if `pgbx jobs` is empty too, the worker is not running (`shared_preload_libraries`).

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
A human who wants a browser app (overview, restore helper, read-only query) can run `pgbx serve --profile <p>`
themselves: it opens a local page with a per-run token. Suggest it; never start it for your own use (the CLI's
`--json` answers everything it shows).
Fallback (SQL, per database): `psql -XAtq -d myapp -c "SELECT json_agg(h) FROM (SELECT id, kind, state, who, trigger, coalesce(finished, requested_at) AS at, error FROM pgbx.history ORDER BY id DESC LIMIT 50) h"`

**Expected response:** startup `{"ok":true,"url":..,"role":..,"warnings":[]}`; timeline rows `{database, kind, state, who, trigger, at, s3_key, error, params}`
newest first (kinds include backup, restore, verify, config, pause, resume, scope, download_url).

**Common errors:** `refusing (--strict): role ... is a SUPERUSER` / `can execute pgbx.backup_now()` → use a
`pgbx_viewer` login; a non-loopback `--listen` prints a warning — exposing it is the human's decision. History
older than `pgbx.audit_days` (default 30) is pruned except kept backups, open gaps and the newest row of each kind.

**User-visible formatting:** the URL, then one line per relevant row: "<at> <database> <kind> <state> by <who>".

### STAT-R-7: Inspect the server with a read query

**When to use:** any question the other recipes do not answer and that a SELECT can: table sizes, row counts,
connections, settings, replication, "what is in pgbx.history". Prefer this over ssh + psql or any shell.
First check the database's memory (MEM-R-1): a saved question in `memories.md` is reused as written, and table and
column names come from `tables.md`; if neither has them, look them up in `information_schema` instead of guessing.

**Command:**
```bash
pgbx query "SELECT datname, pg_size_pretty(pg_database_size(datname)) AS size FROM pg_database" --profile prod --json
pgbx query "SHOW max_connections" --profile prod --json
pgbx query "SELECT state, count(*) FROM pg_stat_activity GROUP BY 1" --db myapp --max-rows 200 --timeout 10s --profile prod --json
```
Fallback (SQL): `psql -XAtq -d myapp -c "SELECT json_agg(t) FROM (<the SELECT>) t"`

**Expected response:** `{columns:[{name,type}], rows:[{...}], row_count, truncated, database, user, profile_used}`.
Numbers, booleans and nulls are typed; SHOW/EXPLAIN values are text. `truncated: true` → narrow the query or raise `--max-rows`.

**Common errors:** `runs SELECT-style statements only` / `refuses 'INSERT'` / `refuses pg_terminate_backend(): it has
side effects` / `exactly one statement` → the guard; do NOT try to get around it (no rewording, no other
function, no psql) — use the matching pgbx command or ask the human. The guard is best-effort, not a security
boundary: never run anything that changes data through it. `statement timeout` → narrow it or raise `--timeout`.

**User-visible formatting:** the answer in one or two sentences, then a small table of the rows that matter.

### STAT-R-8: Job queue — what runs, what waits, why, how long

**When to use:** "what is pgbx doing", "why hasn't my backup started", "is the restore running", "how long will it
take", "cancel that backup", "job queue", "pgbx jobs".

**Command:**
```bash
pgbx jobs --json                                   # the whole server's queue (admin database)
pgbx jobs cancel 42 --db myapp --yes --json        # GUARDED: ask the human first; stops a running job too
```
Fallback (SQL): `psql -XAtq -d postgres -c "SELECT json_agg(q) FROM pgbx.server_queue q"`;
per database `psql -XAtq -d myapp -c "SELECT row_to_json(e) FROM pgbx.job_eta(42) e"`.

**Expected response:** `jobs[]` with `database, job_id, kind, state (running|cancelling|queued|deferred), position,
slot (0 = restore lane), detail (why it waits), progress ('41 % · ~9 min left' / 'queued, #2 in line: starts ~14:05,
takes ~18 min'), eta_start, eta_finish`; `slots` = `max_concurrent_jobs`, `restore_lane`. `cancel` returns a message;
a running job ends as `cancelled` within seconds, with nothing left in S3.

**Common errors:** `refusing without --yes` → by design; confirm with the human. `job id N exists in several
databases` → add `--db`. `only a queued or running job can be cancelled` → it already finished.

**User-visible formatting:** one line per job: "<db> #<id> <kind> <state> — <progress or detail>".

### STAT-R-9: Load gate — is the server busy, would backups wait

**When to use:** "is the server busy", "will a backup hurt the app", "why was the backup deferred", "load gate",
"pgbx load", before a manual backup during business hours.

**Command:**
```bash
pgbx load --json                                   # last sample, thresholds, per-database gate and counts
pgbx load --db myapp --json                        # + that database's recent gated jobs
pgbx load --gate on --db myapp --yes --json        # GUARDED: defer scheduled backups while busy (ask first)
```
Fallback (SQL): `psql -XAtq -d postgres -c "SELECT row_to_json(c) FROM (SELECT load_at, load_busy, load_reasons FROM pgbx.server_capacity) c"`

**Expected response:** `sample.load_busy` + `load_reasons` ('12 active sessions > 4, 900 tps > 200'); `settings.load_gate`
(default `shadow`: records `would_defer`, never delays); `databases[]` with `load_gate, would_defer_7d, deferred_7d,
forced_7d`; `deferred_jobs[]`.

**Common errors:** `--gate needs --db` → the gate is set per database. `no sample yet` → wait one poll.

**User-visible formatting:** one line: "Server <busy: reasons | quiet>; gate <mode>; last 7 days <n> would have waited,
<n> deferred, <n> forced." A manual backup on a busy server still starts: say so and offer the quiet window (POL-R-6).
