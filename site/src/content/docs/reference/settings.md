---
title: Settings
description: Every pgbx.* setting in postgresql.conf, with its default.
sidebar: { order: 2 }
---

All settings below take effect on reload (`SELECT pg_reload_conf();`). Only `shared_preload_libraries` needs a restart.

## S3 and server

| setting | default | what |
|---|---|---|
| `pgbx.s3_endpoint` | *(none)* | S3 endpoint URL |
| `pgbx.s3_bucket` | *(none)* | bucket for all backups of this server |
| `pgbx.s3_region` | `us-east-1` | S3 region |
| `pgbx.server_name` | hostname | top-level folder for this server in the bucket |
| `pgbx.credentials_file` | `/etc/pgbx/s3.credentials` | `access_key_id=` and `secret_access_key=` lines; readable by `postgres` only |
| `pgbx.socket_dir` | `/var/run/postgresql` | Unix socket the worker connects through |
| `pgbx.admin_db` | `postgres` | database that holds `server_overview` (read with `overview()` / `doctor()`) |
| `pgbx.poll_seconds` | `5` | how often the worker looks for new databases and due jobs (1–3600) |

## Per-database backups

| setting | default | what |
|---|---|---|
| `pgbx.max_days_limit` | `90` | ceiling on any database's `max_days` (1–36500) |
| `pgbx.dump_compression` | `auto` | `pg_dump --compress`; `auto` = zstd:3 with pg_dump 16+, gzip level 6 before; or e.g. `lz4`, `gzip:6`, `none`. The newest installed pg_dump/pg_restore is used |
| `pgbx.audit_days` | `30` | days of `history` (the audit trail) kept; kept backups, queued/running jobs and the newest row of each kind are never pruned |
| `pgbx.alert_command` | *(none)* | shell command run for every failed job; JSON on stdin, env `PGBX_DATABASE`, `PGBX_KIND`, `PGBX_JOB_ID`, `PGBX_ERROR`, `PGBX_SERVER` |

## Resource caps

pg_dump / pg_restore run as polite child processes so a backup never takes more than one core and never waits behind
your app's locks.

| setting | default | what |
|---|---|---|
| `pgbx.job_nice` | `10` | CPU niceness of pg_dump / pg_restore (0–19); never raises priority |
| `pgbx.job_ionice` | `best-effort-7` | IO priority on Linux: `none`, `idle`, `best-effort-0` … `best-effort-7` (anything else = `best-effort-7`; `idle` can starve on a busy disk) |
| `pgbx.dump_lock_timeout` | `5s` | how long pg_dump waits for its table locks (0–10min); if DDL holds one, the backup is retried later instead of queueing behind it |
| `pgbx.dump_lock_timeout_forced` | `60s` | the same once a retried backup reached its deadline; if it still times out, the backup fails and alerts |
| `pgbx.defer_backoff` | `1,2,4,8,15` | minutes between retries (each 1–60, the last repeats) |
| `pgbx.max_defer` | `4h` | a retried backup runs anyway this long after it was queued (0–24h), never later than one schedule interval |
| `pgbx.max_defer_first` | `15min` | the same for a new database's first backup |
| `pgbx.dump_compression_busy` | `auto` | `--compress` for a backup forced to run at its deadline; `auto` = zstd:1 with pg_dump 16+, gzip level 1 before |
| `pgbx.upload_kbps` | `0` | upload cap per job in KiB/s; 0 = unlimited |
| `pgbx.download_kbps` | `0` | restore download cap per job in KiB/s; 0 = unlimited |
| `pgbx.restore_synchronous_commit` | `off` | `synchronous_commit` for pg_restore; the target is a new database, so a crash just means restoring again |
| `pgbx.doctor_long_job` | `1h` | `doctor()` warns (`long_running_job`) about a pg_dump / pg_restore running longer than this; 0 = never |

The child connections show in `pg_stat_activity` as `pgbx_dump`, `pgbx_restore` and `pgbx_verify`.

## Removed in 0.5.0

`cluster_backups`, `cluster_schedule`, `cluster_retention_full`, `cluster_full_every`, `cluster_process_max`,
`restore_from_system_id`, `wal_queue_max`, `wal_alert_after`, `wal_alert_size`, `wal_gap_margin`, `work_dir`,
`stop_command`, `start_command`. Remove them from `postgresql.conf`. See [Upgrading](../../guides/upgrading/).

## Per-database defaults

Stored in `pgbx.config`, one row per database. Change them with SQL, not settings.

| | default |
|---|---|
| schedule | daily at 02:00 (`0 2 * * *`) |
| max_backups | 14 |
| max_days | 90 |
| verify schedule | weekly on sunday at 04:00 |
| data scope | every row |
| path | database name |
