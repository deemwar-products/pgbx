# Changelog

## 0.6.0 (unreleased)

- **Resource caps** (ADR 0001 §3): pg_dump / pg_restore run at `pgbx.job_nice` (10) and, on Linux, IO priority
  `pgbx.job_ionice` (best-effort-7), set before exec; their connections are named `pgbx_dump` / `pgbx_restore` /
  `pgbx_verify`.
- **Never queued behind DDL:** pg_dump waits at most `pgbx.dump_lock_timeout` (5s) for its table locks; on a timeout
  the backup stays queued and is retried with `pgbx.defer_backoff`, until `pgbx.max_defer` (4h, first backup 15min,
  never past one schedule interval). Then it runs `forced` with `pgbx.dump_lock_timeout_forced` (60s) and
  `pgbx.dump_compression_busy`, and fails + alerts if it still cannot lock.
- pg_restore runs with `synchronous_commit=off` (`pgbx.restore_synchronous_commit`).
- Bandwidth caps `pgbx.upload_kbps` / `pgbx.download_kbps` (KiB/s, 0 = unlimited).
- doctor(): `long_running_job` (`pgbx.doctor_long_job`, 1h).
- `backup_now()` / `verify_now()` return the job already queued instead of adding another (`pgbx.coalesce_manual`,
  on); new `pgbx.cancel(job_id)` cancels a queued job, or a **running** one: its child is killed, the upload aborted
  (nothing left in S3), a half-restored database dropped, no alert; state `cancelled`.
- **One server-wide job queue** (ADR 0001 §0): the worker supervises jobs as child processes instead of blocking on
  them, picks restore > manual backup > scheduled backup > restore test > prune, then oldest, then round-robin over
  databases; `pgbx.max_concurrent_jobs` (1) enforced by advisory-lock slots in the admin database; `pgbx.restore_lane`
  (on) gives restores their own slot. `pgbx.overrun_policy` (`skip`, or `catch_up`) with `pgbx.overrun_max_gap`
  (1.5 intervals): a dump longer than its interval no longer runs back to back (`params.skipped_slots`).
  doctor(): `dump_longer_than_interval`. Restart recovery and orphan-upload abort keep working (crash e2e).
- `pgbx.server_queue` (admin database) and status() `running_job`, `next_job`, `queue_position`, `waiting_reason`
  say what runs and why a job waits; CLI `pgbx jobs [cancel ID --yes]`; a queue card in `pgbx ui`; `pgbx ui` and
  `--wait` know the `cancelled` state.
- **Time estimates** (§4): a NOTICE on `backup_now()` / `verify_now()` / `restore()` with start, duration, size,
  confidence and the bottleneck; `pgbx.job_eta(job_id)`; live progress (`history.bytes` after every 16 MiB) in
  status() `job_progress` / `job_eta`, `pgbx jobs` and the UI; a daily cpu probe and network / disk rates in
  `pgbx.server_capacity`; doctor(): `capacity`, `eta_accuracy`. Settings `pgbx.eta_samples`, `pgbx.eta_default_mbps`,
  `pgbx.eta_calibrate`.
- **Quiet-window suggestion** (§2): activity per hour of the week (UTC) learned from `pg_stat_database` deltas into
  `pgbx.activity_hourly` (`pgbx.activity_sampling`, `pgbx.activity_decay`); `pgbx.suggest_window()`, status()
  `suggested_schedule`, CLI `pgbx schedule suggest [--apply]`, a suggestion card with copy-to-apply buttons in the
  UI, doctor(): `schedule_in_quiet_window` (`pgbx.doctor_busy_ratio`, `pgbx.suggest_min_days`). Never applied by
  itself.
- **Load gate** (§1), default **shadow**: one load sample per poll (active sessions, tps, long writers, replica lag,
  load average; `pgbx.busy_*`), `params.would_defer` on jobs it would have held back, never a delay. Per database
  `configure(load_gate => 'on')` (new `load_gate` argument) defers scheduled backups / restore tests with
  `pgbx.defer_backoff` up to `pgbx.max_defer`, then runs them `forced`. Human jobs get a NOTICE that they compete with
  the app (`pgbx.gate_manual_jobs`). CLI `pgbx load [--gate ...]`, a load card in the UI, doctor(): `load_gate`,
  `forced_backups_7d`.
- `bench/load_overhead.sh` + `bench/RESULTS.md`: pgbench TPS and latency with no backup, an uncapped and a capped one.
- Fix: multipart uploads cut off by a crash are aborted at the next worker start even where the S3 listing of
  open uploads cannot be parsed (RustFS): the worker keeps its own list (`pgbx_open_uploads` in the data directory).
- Fix: a shutdown during a restore download no longer hangs (the writer returned `Interrupted`, which `write_all` retries).
- Schema update script `pgbx--0.5.0--0.6.0.sql` (the worker applies it by itself).

## 0.5.0

First public release.

- Per-database backups: every database (and `template1`) gets the extension; `pg_dump`/`pg_restore` streamed to
  and from S3. No `archive_mode` or extra package needed.
- `pgbx backups --from-s3` and `pgbx db-restore --from-s3 [--backup KEY | --time TS]` — restore onto a new
  server without the extension (its objects are skipped), streamed with resumable download.
- PostgreSQL 13–18. `pgbx.dump_compression` defaults to `auto` (zstd:3 with pg_dump 16+, gzip level 6 before);
  the newest installed pg_dump/pg_restore is used.
- The worker runs `ALTER EXTENSION pgbx UPDATE` itself when a database or `template1` is older than the library.
- doctor(): extension loaded, s3 settings, credentials file, database backups, restore tests, workers,
  archive_mode (info), replication_slots.
- Docker images take a `PG_MAJOR` build arg.
