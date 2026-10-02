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
  on); new `pgbx.cancel(job_id)` cancels a queued job (state `cancelled`). Cancelling a running job is not in 0.6.0.
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
