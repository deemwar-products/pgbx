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
| `pgbx.credentials_file` | `/etc/pgbx/s3.credentials` | a keys file: `access_key_id=` and `secret_access_key=` lines, readable by `postgres` only. Or `aws-default` (or empty): no keys file, the AWS default chain, see [S3 credentials without a keys file](#s3-credentials-without-a-keys-file) |
| `pgbx.socket_dir` | `/var/run/postgresql` | Unix socket the worker connects through |
| `pgbx.admin_db` | `postgres` | database that holds `server_overview` (read with `overview()` / `doctor()`) |
| `pgbx.poll_seconds` | `5` | how often the worker looks for new databases and due jobs (1–3600) |

### S3 credentials without a keys file

`pgbx.credentials_file = 'aws-default'` (`sudo pgbx setup server --credentials aws-default`): the worker, its
jobs, `wal-push` / `wal-get` and the CLI take credentials from the first of these that is configured. A configured
source that fails is an error; pgbx does not quietly move on to the next.

| order | source | where it applies |
|---|---|---|
| 1 | `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` (+ `AWS_SESSION_TOKEN`) | the CLI only; the extension never takes keys from the Postgres server's environment |
| 2 | web identity (EKS IRSA): `AWS_WEB_IDENTITY_TOKEN_FILE` + `AWS_ROLE_ARN` → STS `AssumeRoleWithWebIdentity` | everywhere |
| 3 | container credentials (ECS task role, EKS Pod Identity): `AWS_CONTAINER_CREDENTIALS_RELATIVE_URI` / `_FULL_URI` | everywhere |
| 4 | the **EC2 instance role through IMDSv2**: a session token from `PUT /latest/api/token`, then the role's credentials with it | everywhere |

- **IMDSv2 only.** If the metadata service gives no session token (an IMDSv1-only endpoint), pgbx refuses and
  says so; it never falls back to IMDSv1. `AWS_EC2_METADATA_SERVICE_ENDPOINT` moves the endpoint,
  `AWS_EC2_METADATA_DISABLED=true` skips it. Postgres in a container on EC2 needs the instance's metadata
  hop limit at 2.
- **Temporary credentials** are cached per process and fetched again 5 minutes before they expire, or when S3
  refuses them (403 `ExpiredToken`). A long multipart upload or a resumed download takes the new ones between
  parts, so it runs across a refresh. The worker logs `s3 credentials from instance role via IMDSv2 (role NAME),
  temporary, valid until ...` each time; no key, secret or token is ever logged or stored.
- The role needs, on the bucket: `s3:ListBucket` (and `s3:ListBucketMultipartUploads` for the crash clean-up) on
  `arn:aws:s3:::BUCKET`, and `s3:PutObject`, `s3:GetObject`, `s3:DeleteObject`, `s3:AbortMultipartUpload`,
  `s3:ListMultipartUploadParts` on `arn:aws:s3:::BUCKET/*`.
- `doctor()` → `s3 credentials` shows the source in use (`file`, `env`, `web-identity`, `ecs`, `instance-role`), the
  role and until when; when nothing works, why and what to attach.
- Download links (`pgbx.download_url`) signed with temporary credentials stop working when those credentials
  expire (for an instance role at most ~6 hours), whatever interval was asked for.
- CLI: `--credentials-file aws-default`, or leave `--credentials-file` out: `backups --from-s3`,
  `db-restore --from-s3` and `pitr list|restore` then use the same chain.

## Per-database backups

| setting | default | what |
|---|---|---|
| `pgbx.max_days_limit` | `90` | ceiling on any database's `max_days` (1–36500) |
| `pgbx.dump_compression` | `auto` | `pg_dump --compress`; `auto` = zstd:3 with pg_dump 16+, gzip level 6 before; or e.g. `lz4`, `gzip:6`, `none`. The newest installed pg_dump/pg_restore is used |
| `pgbx.audit_days` | `30` | days of `history` (the audit trail) kept; kept backups, queued/running jobs and the newest row of each kind are never pruned |
| `pgbx.alert_command` | *(none)* | shell command run for every failed job; JSON on stdin, env `PGBX_DATABASE`, `PGBX_KIND`, `PGBX_JOB_ID`, `PGBX_ERROR`, `PGBX_SERVER` |
| `pgbx.encryption_key_file` | *(none)* | encrypt every dump and its roles file with AES-256-GCM before upload; a file with 32 random bytes as base64 or hex, owned by `postgres`, `chmod 600` (refused when group or others can read it). Empty = no encryption. See [Encrypting backups](../../guides/encryption/) |
| `pgbx.backup_role_passwords` | `off` | keep role password hashes in the roles file stored next to each dump (`pg_dumpall --globals-only`; off = `--no-role-passwords`). Turn encryption on first. See [Roles with every backup](../../guides/roles/) |
| `pgbx.notify` | *(none)* | notification channels **by name**: `slack:NAME, telegram:NAME, webhook:NAME, email:NAME` (a bare `slack` = name `default`). A URL here is refused and never echoed. See [Notifications and metrics](../../guides/notifications/) |
| `pgbx.notify_secrets_file` | *(none)* | the URLs / tokens / SMTP settings of the `pgbx.notify` channels (`slack.ops.url = ...`); owned by `postgres`, `chmod 600` (refused when group or others can read it). Never put a URL in a setting: settings are readable by every role |

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

## Point-in-time restore (optional)

Off by default; whole server. `pgbx setup pitr --yes` turns it on (see
[Point-in-time restore](../../guides/point-in-time-restore/)). `pgbx.pitr` itself is a reload setting, but
`archive_mode` needs one Postgres restart.

| setting | default | what |
|---|---|---|
| `pgbx.pitr` | `off` | archive WAL with `pgbx wal-push` and take scheduled base backups; needs `archive_mode = on` and `archive_command = '<pgbx> wal-push %p'` (both written by `pgbx setup pitr`) |
| `pgbx.pitr_schedule` | `daily at 01:00` | when base backups are queued (same forms as `set_schedule()`); they run as jobs of the server-wide queue |
| `pgbx.pitr_retention` | `7 days` | restore window: every base backup needed to reach any moment of the last N days is kept (the newest one that stopped before the cutoff included), the newest base backup always; older base backups and WAL before the oldest kept backup's start are deleted (`.history` files kept) |
| `pgbx.wal_queue_max` | `4GB` | when WAL waiting to be archived exceeds this (S3 down), `wal-push` **drops** WAL instead of filling the disk; each drop is logged and recorded as a **gap**; `off` = never drop |
| `pgbx.wal_gap_margin` | `60s` | restores are also refused this long before a gap's last safe moment |
| `pgbx.wal_alert_after` | `15 min` | alert when the oldest WAL waiting to be archived is older than this |
| `pgbx.wal_alert_size` | `2GB` | alert when this much WAL waits to be archived; `off` = never |
| `pgbx.work_dir` | `<data_directory>/../pgbx` | PITR state: `pgbx-wal.conf` (0600, no keys: only the credentials file's path, or `aws-default`), spool, drop log; must be **outside** the data directory |
| `pgbx.cli_path` | `/usr/local/bin/pgbx` or `/usr/bin/pgbx` | the `pgbx` program the worker runs for base backups and gap uploads |

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
| GFS retention | none (`set_retention(gfs => '7d,4w,12m')`) |
