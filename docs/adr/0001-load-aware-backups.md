# ADR 0001 — Load-aware backups: idle gate, quiet-window suggestion, resource caps

- **Status:** Proposed
- **Date:** 2026-10-02
- **Deciders:** pgbx maintainers

## Context

Owner's concern: *"whenever pg_dump or restore happens when more users are active it will unnecessarily hurt
performance — do an idle check, if there are logs suggest a good time, and ensure we are not taking too much process."*

How it works today (branch `load-aware-backups`, base `main`):

- One background worker, `pgbx scheduler`, registered in `src/lib.rs:74-78`, wakes every `pgbx.poll_seconds`
  (`src/worker.rs:64`) and calls `tick()` (`src/worker.rs:123`), which walks every connectable database in name order
  and calls `serve_db()` for each (`src/worker.rs:150-152`).
- `serve_db()` queues a `backup` row in `pgbx.history` when `is_due(schedule, last)` (`src/worker.rs:300-304`;
  cron math in `src/schedule.rs:59-61`, default `0 2 * * *`), with trigger `first` for a database never backed up —
  that is the "within a minute of CREATE DATABASE" backup. `verify` jobs are queued the same way (`:306-320`).
- Queued jobs run **synchronously, oldest first**, inside the same loop (`src/worker.rs:325-339`). So concurrency is
  already **one job at a time per server**: there is one worker and it blocks on each `pg_dump`/`pg_restore`.
  This is a property worth keeping and writing down; it is not enforced by any lock today.
- There is **no load check** before a job starts. A job queued by `backup_now()` / `restore()` / the schedule starts
  on the next tick regardless of what the application is doing.
- `backup()` spawns `pg_dump -Fc --compress <auto>` with default OS priority (`src/worker.rs:517-523`); compression
  is `zstd:3` on pg_dump 16+, else gzip 6 (`src/worker.rs:551-560`). Output streams to S3 through our multipart
  loop with 16 MiB parts and no bandwidth limit (`src/transfer.rs:12`, `:56-90`).
- `restore_key_into()` spawns `pg_restore` (single job, no `-j`) into a NEW database, default priority
  (`src/worker.rs:633-657`); `verify` uses the same path (`:680`).
- pg_dump takes `ACCESS SHARE` locks on every table for the whole dump. It does not block app reads/writes, but a
  queued `ALTER TABLE` behind it blocks everything behind *that* — the real way a backup "hurts" an app.
- Status surfaces: `pgbx.server_overview` rows written by `publish_overview()` (`src/worker.rs:401`),
  `pgbx.status()` (`src/lib.rs:248`) and `pgbx.doctor()` (`src/lib.rs:492`).
- **One worker per server, one queue per database.** The extension is installed in every database (own `pgbx`
  schema, `config`, `history`), but there is exactly one worker for the whole server. `pgbx.history` rows with
  `state='queued'` are each database's queue; the worker drains them database by database in name order.
  Consequences today: (a) a restore asked for in `zeta` waits for every queued job in `alpha`..`yotta` first
  (head-of-line blocking); (b) a backup can't be double-queued by the schedule (`pending()`, `src/worker.rs:269`),
  but missed cron slots during a long dump collapse into one catch-up that starts **immediately** after
  (`is_due` on `max(requested_at)`, `src/worker.rs:251-266`), so an hourly schedule with 70-min dumps dumps
  non-stop; (c) `backup_now()` (`src/lib.rs`) is not deduplicated — five calls queue five dumps.

## Decision

Three mechanisms, all inside the existing worker, all off-by-default until the rollout below finishes.

### 0. One server-wide job queue (prerequisite for the gate and caps)

Keep `pgbx.history` in each database as the source of truth (teams see their own jobs; restores copy it), but the
worker schedules **across** databases instead of draining them one by one:

1. Each tick it collects `queued` rows from every database (it already connects to each one) into an in-memory
   list, mirrored into `pgbx.server_queue` in the admin DB so `pgbx overview` / CLI show one server-wide queue.
2. It picks the next job by **priority, then age**: `restore` (a human is waiting) > manual `backup` > scheduled /
   first `backup` > `verify` > `prune`. Ties: round-robin over databases so one noisy DB cannot starve the rest.
3. **Lanes.** `pgbx.max_concurrent_jobs` (default 1) is the total. `pgbx.restore_lane` (default on) reserves one
   extra slot for restores only, so a restore never waits behind a 3-hour dump; it is a separate process into a
   NEW database and runs at the same nice/ionice. Set it off on very small servers. Lanes run as child processes
   polled by the worker loop (non-blocking `try_wait`), so the worker keeps ticking, sampling and answering SIGTERM.
4. **Coalescing.** At most one queued backup per database: `backup_now()` while one is queued returns that job's id
   (and upgrades its priority to manual) instead of adding another. Same for `verify_now()`. Restores never coalesce.
5. **Overrun policy** `pgbx.overrun_policy` = `skip` (default) | `catch_up`. `skip`: slots missed while a dump of
   that database was running are dropped; the next run is the next cron slot after the dump **finished**
   (history records `params.skipped_slots`). Guard against the skip making the gap too big: if waiting for that
   slot would leave more than `pgbx.overrun_max_gap` × interval (default **1.5**) since the last good backup's
   start, it runs once right away instead. So hourly + 70-min dump → next run at the next hour (no back-to-back);
   daily dump that overran past 02:00 → it doesn't wait until tomorrow. `catch_up` = today's behaviour.
   `doctor()` warns `dump_longer_than_interval` when the last 3 dumps ran longer than the interval, with the
   suggested longer schedule as `fix`.
6. **Same database, two jobs.** Never two dumps of one DB at once. A restore/verify of DB X may run while X is
   being dumped (different target DB). A dump of the database a restore is writing into is skipped (it is not live).
7. **Cancel.** `pgbx.cancel(job_id)`: `queued` → `cancelled`; `running` → SIGTERM the child, aborts the S3 upload
   (existing path), state `cancelled`. CLI `pgbx jobs` / `pgbx jobs cancel <id>`.

### 1. Load gate before starting a job

Just before a `queued` job flips to `running` (`src/worker.rs:337`), the worker samples load from its existing
admin connection — no extension in the hot path of app queries, ≈1 ms per sample:

| signal | query / source | busy when |
|---|---|---|
| active backends | `count(*) FROM pg_stat_activity WHERE state <> 'idle' AND backend_type = 'client backend' AND pid <> pg_backend_pid() AND application_name NOT LIKE 'pgbx%'` | `> pgbx.busy_active_backends` (default **4**) |
| transaction rate | delta of `sum(xact_commit + xact_rollback) FROM pg_stat_database` between two ticks ÷ seconds | `> pgbx.busy_tps` (default **200**) |
| long writers | any `state='active'` backend with `now()-xact_start > 30s` holding RowExclusive+ locks | always busy (avoid stacking on a migration) |
| replica lag (if primary has standbys) | `max(replay_lag) FROM pg_stat_replication` | `> pgbx.busy_replica_lag` (default **30s**) |
| host load (Linux only, optional) | `/proc/loadavg` 1-min ÷ `available_parallelism()` | `> pgbx.busy_loadavg` (default **0.8**); ignored where unreadable |

The worker tags its own children `application_name=pgbx_dump`/`pgbx_restore` (PGAPPNAME env) so they never count.

**Policy** (per job, state kept in `pgbx.history.params`):

- Busy → leave the job `queued`, set `params.deferred_until = now() + backoff` and `params.defer_reason`;
  backoff 1, 2, 4, 8, 15 min, then every 15 min. Log one line per deferral, not per tick.
- **Deadline:** a scheduled backup is never deferred past `queued_at + pgbx.max_defer` (default **4h**, capped below
  the schedule interval so two never pile up). At the deadline it runs with the resource caps of §3 and the
  history row records `params.forced = true`. A backup is never skipped because the server stayed busy.
- `first` backups (new database) get `max_defer = 15min`: a new DB is usually empty and cheap.
- **Human-requested jobs** (`backup_now()`, `restore()`, `verify_now()`): **warn, don't block.** The SQL function
  runs the same sample synchronously and raises a `NOTICE` ("12 active sessions, 900 tps — this will compete with
  the app; it starts now"); the job runs immediately. CLI prints the same warning and asks `--yes` when interactive.
- Gate state: `pgbx.load_gate = off | shadow | on` (default `shadow`, owner decision 2026-10-02). Clients see it
  in `pgbx status` / `pgbx load` (last sample, `would_defer` count per job, top busy reasons) and enable it with
  `pgbx configure --load-gate on` or `SELECT pgbx.configure(load_gate => 'on')`. `shadow` evaluates and records
  `params.would_defer` but never delays.

### 2. Quiet-window suggestion (learned, never auto-applied)

- Every tick (default 60 s) the worker already connects to each database; it reads `xact_commit + xact_rollback`,
  `tup_inserted+tup_updated+tup_deleted`, `blks_read` from `pg_stat_database` for that DB and adds the delta into
  `pgbx.activity_hourly (dow smallint, hour smallint, samples int, xacts float8, writes float8, active_max int)`.
  168 rows per database, exponentially decayed (`x = 0.9·x + 0.1·sample` per hour-bucket), so it adapts and never
  grows. Stats resets (`stats_reset` changed / negative delta) drop that sample. Time zone: server `TimeZone`.
- Server-wide histogram = sum of the per-database ones, kept in the admin DB (`pgbx.server_activity_hourly`).
- No log parsing needed. Optional later: if `logging_collector` and `log_min_duration_statement` are on, the CLI
  `pgbx schedule suggest --from-logs <dir>` may seed the histogram from logs; out of scope for v1.
- `pgbx.suggest_window(hours int DEFAULT 1) RETURNS TABLE (start_at text, cron text, score float8, confidence text,
  current_schedule text, current_score float8)`: lowest-activity contiguous window (per day; weekly if weekdays
  differ by > 2×), excluding windows already used by other databases' backups on this server to spread load.
  `confidence = 'low'` until ≥ 7 days of samples.
- CLI: `pgbx schedule suggest [--db X]` prints the table plus the one-liner
  `SELECT pgbx.configure(schedule => '30 4 * * *');` ready to copy; `--apply` runs it after a y/N prompt. The worker
  never changes `pgbx.config.schedule` itself (owner decision 2026-10-02: never auto-apply, always show, user executes).
  The web UI (`cli/src/ui.rs`) shows the suggestion with an Apply button that runs the same call.
- `doctor()` adds a check `schedule_in_quiet_window`: warns when the current schedule's hour scores > 3× the
  suggested window, with the configure() call as `fix`.

### 3. Resource caps on the child processes

- **CPU/IO priority:** after `spawn()`, `setpriority(PRIO_PROCESS, pid, pgbx.job_nice)` (default **10**); on Linux
  also `ioprio_set(IOPRIO_WHO_PROCESS, pid, IOPRIO_CLASS_BE, 7)` via `libc::syscall` (`pgbx.job_ionice = 'idle' |
  'best-effort-7' | 'none'`, default `best-effort-7`; `idle` can starve forever on busy disks, so not default).
  Done with `pre_exec` so it applies before pg_dump starts reading. macOS: setpriority only.
  Note the honest limit: the *backend* serving pg_dump's COPY is a Postgres process we do not renice; most CPU of a
  dump is compression in pg_dump itself, which we do cap.
- **Concurrency:** formalise today's one-job-at-a-time: a `pg_try_advisory_lock(hashtext('pgbx_job'))` in the admin
  DB slots `1..pgbx.max_concurrent_jobs` (default 1) around each job, so a second worker or the CLI cannot exceed it.
  `pg_restore -j` / `pg_dump -j` stay unused (they need directory format anyway).
- **Compression:** keep `pgbx.dump_compression` default; under a forced (deadline) run or when the gate saw load,
  use `zstd:1` / gzip 1. Never use zstd `workers=` (multi-threaded) — one core max.
- **Upload bandwidth:** `pgbx.upload_kbps` (default **0** = unlimited) — token bucket in `upload_stream` /
  `download_resumable` (`src/transfer.rs:56`, `:126`) sleeping between 16 MiB parts / reads. Back-pressure on the
  pipe slows pg_dump naturally.
- **Never block app writes:** pg_dump runs with `PGOPTIONS='-c lock_timeout=5s -c statement_timeout=0
  -c idle_in_transaction_session_timeout=0'`: the initial lock acquisition fails fast if a migration holds
  AccessExclusive, the job is re-queued with backoff (counts toward the deadline) instead of queueing behind DDL.
  Its ongoing ACCESS SHARE locks still block DDL; documented, and `doctor()` reports a running dump older than 1h.
  pg_restore into the NEW database: `lock_timeout` irrelevant; `synchronous_commit=off` to cut WAL fsync pressure.
- **Measuring overhead:** `bench/load_overhead.sh` — pgbench `-c 16 -T 300` against a scale-50 DB, run three
  times: no dump, dump at default priority, dump with caps. Record TPS and p95 latency. Budget: **p95 latency
  +≤ 15 %, TPS −≤ 10 %** with caps on. Results committed to `bench/RESULTS.md` per release.

### Config — every knob is a setting, every default is the safe one

Every number in this ADR is a GUC; values quoted in the text above are its defaults. All are `PGC_SIGHUP`
(reload, no restart), defined next to the others in `src/lib.rs:58-69`, validated with min/max bounds so a typo
cannot disable safety (e.g. `max_defer` can't exceed 24h, `job_nice` is clamped 0-19). Defaults follow one rule:
**never lose a backup, never take more than one core, never wait behind app locks.**
The `*_db` columns let a database override a server value in its own migration via `pgbx.configure()`;
the server value is a ceiling where noted.

| GUC | default | range | per-DB override | why this default is safe |
|---|---|---|---|---|
| **gate** | | | | |
| `pgbx.load_gate` | `shadow` | off/shadow/on | `config.load_gate` | records what it would defer, never delays; teams turn `on` themselves |
| `pgbx.busy_active_backends` | 4 | 0-10000 (0 = ignore) | yes | small servers rarely exceed 4 non-idle sessions when quiet |
| `pgbx.busy_tps` | 200 | 0-10^7 (0 = ignore) | yes | |
| `pgbx.busy_long_xact` | 30s | 0-1h (0 = ignore) | no | don't stack on a migration |
| `pgbx.busy_replica_lag` | 30s | 0-1h (0 = ignore) | no | |
| `pgbx.busy_loadavg` | 0.8 | 0-10 (0 = ignore) | no | per-core; ignored where `/proc` is absent |
| `pgbx.defer_backoff` | `1,2,4,8,15` (min) | list, each 1-60 | no | last value repeats |
| `pgbx.max_defer` | 4h | 0-24h, < schedule interval | yes, ≤ server | backups never skipped |
| `pgbx.max_defer_first` | 15min | 0-24h | no | new DBs get protected fast |
| `pgbx.gate_manual_jobs` | `warn` | warn/defer/off | yes | humans decide; `defer` for those who want it |
| **suggestion** | | | | |
| `pgbx.activity_sampling` | on | on/off | yes | one stats read per tick |
| `pgbx.activity_decay` | 0.9 | 0.5-0.99 | no | adapts in ~2 weeks |
| `pgbx.suggest_min_days` | 7 | 1-90 | no | below it, confidence = low |
| `pgbx.doctor_busy_ratio` | 3.0 | 1-100 | no | when doctor warns "busy hour" |
| **resource caps** | | | | |
| `pgbx.job_nice` | 10 | 0-19 | no | lower than the app, never higher |
| `pgbx.job_ionice` | `best-effort-7` | none/best-effort-0..7/idle | no | `idle` can starve forever |
| `pgbx.max_concurrent_jobs` | 1 | 1-8 | no | today's behaviour, now enforced by advisory lock |
| `pgbx.restore_lane` | on | on/off | no | restores never wait behind a dump |
| `pgbx.overrun_policy` | skip | skip/catch_up | yes | no back-to-back dumps |
| `pgbx.overrun_max_gap` | 1.5 | 1.0-10 (× interval) | yes | skipping never stretches RPO by more than half an interval |
| `pgbx.coalesce_manual` | on | on/off | no | spamming backup_now() costs one dump |
| `pgbx.dump_compression` | `auto` (exists) | | no | |
| `pgbx.dump_compression_busy` | `zstd:1` / gzip 1 | | no | cheaper when forced under load |
| `pgbx.upload_kbps` | 0 (unlimited) | 0-10^7 | no | uploads already back-pressure pg_dump |
| `pgbx.download_kbps` | 0 | 0-10^7 | no | |
| `pgbx.dump_lock_timeout` | 5s | 0-10min | no | never queue behind DDL |
| `pgbx.dump_lock_timeout_forced` | 60s | 0-10min | no | at the deadline, try harder then alert |
| `pgbx.restore_synchronous_commit` | off | on/off | no | restore target is a NEW db, safe to redo |
| `pgbx.doctor_long_job` | 1h | 0-24h | no | |

`SHOW pgbx.*` and `pgbx.doctor()` list effective values and flag any non-default that weakens a safety rule.

### Surfacing

- `pgbx.server_overview.state` gains `deferred (busy: 12 active, 900 tps) until 02:16, deadline 06:00`.
- `pgbx.status()` (per database, exists) gains: next due, queued/running job, last gate sample, defer count, deadline,
  suggested window. CLI `pgbx status` reads it.
- `doctor()` adds: `load_gate` (mode, last sample), `forced_backups_7d` (warn if > 2: schedule sits in a busy
  window), `schedule_in_quiet_window`, `long_running_job`.
- History rows keep `params.deferrals`, `params.forced`, `params.load_at_start` for audit.

## Consequences

- Scheduled backups may start up to `max_defer` late; RPO for a daily schedule grows from 24h to ≤ 28h worst case.
  Explicit and visible in history.
- One extra cheap query per tick per DB plus a 168-row table per DB. Stats views are per-backend snapshots — fine.
- Thresholds are absolute numbers; a busy-by-design server (always > 4 active) will defer every time and run at the
  deadline. `doctor()` flags this and the suggestion tells them when to move. Auto-tuning thresholds is deferred.
- Niceness helps on contended CPU; it cannot help a saturated disk on macOS or the Postgres backend side of COPY.
- The `lock_timeout` retry can, under constant DDL, push a backup to the deadline; at the deadline it runs with
  `lock_timeout` raised to 60s and alerts via `pgbx.alert_command` if it still fails.

## Alternatives considered

- **Skip the backup when busy.** Rejected: silent RPO loss; the owner wants protection, not just politeness.
- **Parse Postgres logs for activity.** Rejected as primary: needs `logging_collector`, format varies, privileged
  file access; `pg_stat_database` deltas give the same hourly shape for free. Kept as an optional CLI seed.
- **Auto-move the schedule to the quiet window.** Rejected: teams set schedules in migrations (DESIGN.md
  "Teams change settings in their normal migrations"); silent changes surprise them. Opt-in `--apply` only.
- **cgroups / systemd slices.** Better isolation but needs root and differs per distro/Docker/macOS; out of scope.
  Documented as an operator option.
- **Back up from a replica.** The real fix for heavy servers, but pgbx targets single-server setups; later ADR.
- **Parallel dumps (`-Fd -j N`).** Faster but the opposite of "don't take too much"; and breaks single-stream S3 upload.

## Test plan

- Unit: gate decision table (signals → busy/ok), backoff schedule, deadline math against `schedule::next_after`,
  histogram decay and stats-reset handling, `suggest_window` on synthetic histograms (incl. weekday split).
- e2e (Docker, existing harness): `load_gate=on`, `busy_tps=50`, `max_defer=3min`, `poll_seconds=5`;
  start `pgbench -c 8 -T 600`; `backup_now()` → asserts NOTICE and immediate run; scheduled job → asserts
  `state='queued'` with `defer_reason` for ≥ 1 min, then `state='done'`, `params.forced=true` before deadline + 1 tick.
  Stop pgbench mid-way in a second case → job runs before the deadline with `forced` absent.
- e2e queue: 3-min dump in DB A, `restore()` in DB B starts within one tick (restore lane); with lane off it
  runs next, ahead of A's queued verify. `backup_now()` ×5 → one job id. Hourly schedule + slow dump →
  `skip` leaves a gap and records `skipped_slots`. `cancel()` on a running dump leaves no object in S3.
- e2e: `shadow` mode never delays but records `would_defer`.
- e2e: hold `ACCESS EXCLUSIVE` on a table → dump fails fast on lock_timeout, re-queues, succeeds after release.
- e2e: child `nice` value is 10 (`ps -o ni`), `ioprio` on Linux (`/proc/<pid>/io` via `ionice -p`).
- Bench: overhead budget above, run manually on the self-hosted runners (CI stays manual-only).

## Rollout

1. **Release 1:** resource caps on (they only reduce impact), server-wide queue with `overrun_policy=skip`,
   activity sampling and `suggest_window()`.
2. **Release 2:** load gate with default `shadow`: clients see `would_defer` in `pgbx status` / `pgbx load` / UI and
   turn it `on` per database or server-wide. Defaults for thresholds are retuned from shadow data on our servers.
3. Default `on` only by a later ADR, if shadow data shows deferrals help and deadlines are rarely hit.
