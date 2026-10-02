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
| `pgbx.dump_compression_busy` | `auto` | `--compress` for a backup forced to run at its deadline; `auto` = zstd:1 with pg_dump 16+, gzip level 1 before, or `pgbx.dump_compression` when that is already cheaper (`none`, `0`, `lz4`) |
| `pgbx.upload_kbps` | `0` | upload cap per job in KiB/s; 0 = unlimited |
| `pgbx.download_kbps` | `0` | restore download cap per job in KiB/s; 0 = unlimited |
| `pgbx.restore_synchronous_commit` | `off` | `synchronous_commit` for pg_restore; the target is a new database, so a crash just means restoring again |
| `pgbx.doctor_long_job` | `1h` | `doctor()` warns (`long_running_job`) about a pg_dump / pg_restore running longer than this; 0 = never |

The child connections show in `pg_stat_activity` as `pgbx_dump`, `pgbx_restore` and `pgbx_verify`.

## Job queue

One worker runs every database's jobs from one server-wide queue: restore > manual backup > scheduled / first backup >
restore test > prune, then oldest first, then the database that waited longest. Jobs run as child processes the
worker supervises every second, so it keeps polling (new databases, cancels, SIGTERM) while a dump runs.
`pgbx jobs` shows the queue and why each job waits.

| setting | default | what |
|---|---|---|
| `pgbx.max_concurrent_jobs` | `1` | jobs running at once on this server (1–8); each holds an advisory-lock slot in the admin database, so a second worker cannot exceed it either |
| `pgbx.restore_lane` | `on` | one extra slot for restores only, so a restore never waits behind a long dump; turn off on very small servers |
| `pgbx.coalesce_manual` | `on` | `backup_now()` / `verify_now()` return the job of that kind already queued instead of adding another |
| `pgbx.overrun_policy` | `skip` | schedule slots that passed while a dump of that database ran: `skip` = the next run is the next slot after the dump finished (`params.skipped_slots`); `catch_up` = run once right away |
| `pgbx.overrun_max_gap` | `1.5` | with `skip`: if waiting for that slot would leave more than this many schedule intervals since the last good backup **finished**, run right away (1.0–10) |

Never two dumps of one database at once; a restore or restore test of a database may run while it is being dumped.

## Time estimates

Every job gets an estimate when it is created (a `NOTICE`), and live progress while it runs (`job_eta()`,
`status()`, `pgbx jobs`, the UI).

| setting | default | what |
|---|---|---|
| `pgbx.eta_samples` | `5` | recent jobs of the same kind per database the speed comes from (1–50) |
| `pgbx.eta_default_mbps` | `20` | MB/s assumed before anything was measured; deliberately slow, so first estimates err long (1–10000) |
| `pgbx.eta_calibrate` | `on` | once a day (and after a reload), with no job running, time one niced core compressing up to 64 MiB of the largest table's pages (~1 s) |

Network speed comes from the last 20 upload parts / downloads (no extra traffic); disk speed from `blk_read_time`
(needs `track_io_timing`). `doctor()` shows them (`capacity`) and how good past estimates were (`eta_accuracy`).

## Quiet window

The worker reads `pg_stat_database` for every database each poll and learns the activity per hour of the week into
`pgbx.activity_hourly` (decayed averages, at most 336 rows per database; hours are UTC, like schedules).
`suggest_window()` / `pgbx schedule suggest` name the quietest window. It is **never applied by itself**.

| setting | default | what |
|---|---|---|
| `pgbx.activity_sampling` | `on` | one stats read per database per poll |
| `pgbx.activity_decay` | `0.9` | weight of the past in each hour's average (0.5–0.99); adapts in about two weeks |
| `pgbx.suggest_min_days` | `7` | days of samples before a suggestion has `high` confidence (1–90) |
| `pgbx.doctor_busy_ratio` | `3.0` | `doctor()` (`schedule_in_quiet_window`) warns when the schedule's hour is busier than an average hour and this many times busier than the suggested window (1–100) |

## Load gate

Before a scheduled backup or restore test starts, the worker looks at the server's load (one sample per poll; pgbx's
own sessions never count). The default is `shadow`: it records `params.would_defer` and never delays. With `on` a
busy server defers the job with `pgbx.defer_backoff`, never past `pgbx.max_defer` (then it runs `forced`).
Set it per database with `SELECT pgbx.configure(load_gate => 'on')` or `pgbx load --gate on --db X --yes`.

| setting | default | what |
|---|---|---|
| `pgbx.load_gate` | `shadow` | `off`, `shadow` (record only), `on` (defer while busy); a database's `configure(load_gate => ...)` wins |
| `pgbx.busy_active_backends` | `4` | busy when more client sessions than this are not idle (0 = ignore) |
| `pgbx.busy_tps` | `200` | busy above this many transactions per second, all databases (0 = ignore); pgbx's own polling adds a few per database |
| `pgbx.busy_long_xact` | `30s` | busy while a writing transaction is open longer than this (0 = ignore): never stack a dump on a migration |
| `pgbx.busy_replica_lag` | `30s` | busy while a standby replays more than this behind (0 = ignore) |
| `pgbx.busy_loadavg` | `0.8` | busy above this 1-minute load average per core (Linux; 0 = ignore) |
| `pgbx.gate_manual_jobs` | `warn` | `backup_now()` / `verify_now()` / `restore()`: `warn` = a NOTICE that it competes with the app, it starts anyway; `defer` = gated like scheduled jobs (restores never are); `off` |

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
| load gate | the server's `pgbx.load_gate` |
