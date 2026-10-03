---
title: SQL functions
description: Every public function and view in the pgbx schema.
sidebar: { order: 1 }
---

Per-database functions run inside the database they manage. `overview()` and `doctor()` run only in the admin
database (`pgbx.admin_db`, default `postgres`). Jobs are rows in `pgbx.history`
(`queued` → `running` → `done` | `failed` | `expired` | `cancelled`).

Kinds: `backup`, `restore`, `verify`, `prune`, `config`, `pause`, `resume`; with point-in-time restore on, the admin
database also holds the server-wide `base_backup` (a job of the queue), `wal_gap` and `wal_archive` (open incidents:
state `running` while open, `done` once closed; trigger `wal_gap` for the base backup a gap queued). `status()` never
counts these server-wide rows as the admin database's own.

## Status (viewer)

### `status()`
```sql
pgbx.status() RETURNS TABLE (
  database name, state text, schedule text, cron text, next_backup_at timestamptz,
  last_backup_at timestamptz, last_backup_age interval, last_backup_size text, last_backup_key text,
  backups_kept bigint, retention text, data_scope text, verify_schedule text, last_verified_at timestamptz,
  last_verify_result text, last_error text, last_error_at timestamptz, paused_reason text, paused_at timestamptz,
  queued_jobs bigint, location text, running_job bigint, next_job bigint, queue_position int, waiting_reason text,
  job_progress text, job_eta text, suggested_schedule text, load_gate text, last_load text, would_defer_7d bigint)
```
Who: `pgbx_viewer`. One row: "are we backed up?" `retention` reads like `max 14 backups, max 90 days, gfs 7d,4w,12m`. The last columns say what runs (`running_job`, `job_progress`
like `41 % · ~9 min left`), what is next and why it waits (`waiting_reason`, e.g. `waits for a job slot: 1 of 1 in use
(shop backup #12)`), when (`job_eta`), the quietest schedule (`suggested_schedule`, never applied by itself) and the
load gate (`load_gate`, `last_load`, `would_defer_7d`).
```sql
SELECT state, last_backup_at, next_backup_at FROM pgbx.status();
```

### `backups` (view)
Columns: `id, taken_at, age, trigger, size, bytes, s3_key`. Every kept backup, newest first. Who: viewer.
```sql
SELECT * FROM pgbx.backups;
```

### `history` (table)
Every backup, restore, verify, config change, pause, prune. Who: viewer (read). Since 0.6 a backup's `params` also
carry `encrypted` (true/false), `globals` (the S3 key of its roles file), `roles` (the roles this database references,
a JSON array) or `globals_error`; a restore with `with_roles` carries `params.roles` = `created`, `existing`,
`out_of_scope`, `skipped`, `failed`.
```sql
SELECT id, kind, state, error FROM pgbx.history ORDER BY id DESC LIMIT 10;
```

### `overview()` — admin database
```sql
pgbx.overview() RETURNS TABLE (database name, state text, schedule text, last_backup_at timestamptz,
  last_backup_age interval, last_backup_size text, next_backup_at timestamptz, backups_kept bigint,
  last_verify text, last_error text, seen_at timestamptz)
```
Who: viewer. One row per database on the server. The table behind it, `server_overview`, also carries the Prometheus
metrics columns `last_backup_bytes`, `failures_total` (failed backups / restores / restore tests in the history),
`queued_jobs`, `last_verify_ok`, `last_backup_encrypted` (read by `pgbx metrics` and `GET /metrics`).

### `doctor()`
```sql
pgbx.doctor() RETURNS TABLE (name text, ok bool, detail text, fix text)
```
Who: viewer (runs as `SECURITY DEFINER`). Health checks: `extension loaded`, `s3 settings`, `s3 credentials` (the source in use: file, env, web-identity, ecs, instance-role),
`database backups` (age vs schedule), `restore tests`, `workers`, `archive_mode` (info only; while `pgbx.pitr` is
off), `replication_slots` and `long_running_job` (a pg_dump / pg_restore running longer than `pgbx.doctor_long_job`).
With `pgbx.pitr = on` the `archive_mode` row is replaced by `pitr archiving` (archive_mode on, archive_command is
`pgbx wal-push`, not failing), `pitr base backups` (newest within its schedule) and `pitr gaps` (no open WAL gap).

Since 0.6.0 also `dump_longer_than_interval`, `capacity` (info: cpu / disk / network speed the estimates use),
`eta_accuracy`, `schedule_in_quiet_window`, `load_gate` (info) and `forced_backups_7d`.

### `job_eta(job_id bigint)`
```sql
pgbx.job_eta(job_id bigint) RETURNS TABLE (queue_position int, eta_start timestamptz, eta_finish timestamptz,
  est_bytes bigint, done_bytes bigint, confidence text, progress text)
```
Who: viewer. A running job's finish comes from its real throughput (the worker records `history.bytes` after every
16 MiB); a queued one's start from its place in the server-wide queue. `confidence`: `high` (≥ 3 own runs, steady),
`medium` (this server's runs), `low` (measured capacity or `pgbx.eta_default_mbps`), `measured` (running).
```sql
SELECT progress, eta_finish FROM pgbx.job_eta(42);   -- 41 % · ~9 min left
```

### `suggest_window(hours int DEFAULT 1)`
```sql
pgbx.suggest_window(hours int DEFAULT 1) RETURNS TABLE (start_at text, cron text, score float8, confidence text,
  current_schedule text, current_score float8, window_hours int, est_duration text, days_sampled float8, apply_sql text)
```
Who: viewer. The quietest window to back up, from the activity learned per hour of the week (server-wide when known).
`score` / `current_score`: activity relative to an average hour (1.0). The window is long enough for the estimated
dump, avoids hours another database's backup starts in, and is weekly when weekdays differ by more than 2×.
`confidence`: `none`, `low` (< `pgbx.suggest_min_days`), `high`. Hours are UTC. **Never applied by itself**:
```sql
SELECT apply_sql FROM pgbx.suggest_window();   -- SELECT pgbx.configure(schedule => '0 3 * * *');
```

### `server_queue`, `server_capacity`, `activity_hourly` (tables)
`server_queue` (admin database): every running, queued and deferred job of the server with `position`, `slot`
(0 = restore lane), `detail` (why it waits), `progress`, `eta_start`, `eta_finish`. `server_capacity`: what the
estimates and the load gate are based on (last load sample included). `activity_hourly`: the learned activity.
Who: viewer (read).

### `pitr_status()` — admin database
```sql
pgbx.pitr_status() RETURNS TABLE (enabled bool, state text, archive_mode text, schedule text, retention text,
  last_base_backup_at timestamptz, last_base_backup text, base_backups int, restorable_from timestamptz,
  wal_archived_until timestamptz, open_gaps bigint, gaps text, backlog_segments bigint, backlog_bytes bigint,
  last_error text, location text)
```
Who: viewer (runs as `SECURITY DEFINER`). "Can I restore this server to any moment, and from when?" `state`: `off`,
`restart needed (archive_mode is off)`, `gap: ...`, `archiving failing`, `first base backup running`,
`waiting for first base backup`, `base backups failing`, `active`. See
[Point-in-time restore](../../guides/point-in-time-restore/).

### `pitr_state` (table) — admin database
`system_id`, `work_dir`, `base_backups` (the `backup.json` of every kept base backup, oldest first), `updated_at`.
Written by the worker. Who: viewer (read).

### `rowless_tables()`
```sql
pgbx.rowless_tables() RETURNS TABLE (table_name text)
```
Who: viewer. Tables whose rows the next backup skips.

### `to_cron(schedule text) RETURNS text`
Who: viewer. Shows how a schedule text is read: `SELECT pgbx.to_cron('every 6 hours');`

## Policy (admin)

### `configure(...)`
```sql
pgbx.configure(schedule text DEFAULT NULL, max_backups int DEFAULT NULL, max_days int DEFAULT NULL,
                      enabled bool DEFAULT NULL, path text DEFAULT NULL, load_gate text DEFAULT NULL) RETURNS pgbx.config
```
Who: `pgbx_admin`. Only the arguments you pass change. Made for migrations. `load_gate`: `off` | `shadow` | `on` for
this database, `default` = follow `pgbx.load_gate`.
```sql
SELECT pgbx.configure(schedule => 'every 6 hours', max_backups => 28, max_days => 30);
SELECT pgbx.configure(enabled => false);   -- opt out (recorded)
SELECT pgbx.configure(load_gate => 'on');  -- defer scheduled backups while the server is busy (up to pgbx.max_defer)
```

### `set_schedule(schedule text) RETURNS text`
Who: admin. `'every 15 minutes'`, `'every 1 hour'`, `'hourly'`, `'daily'`, `'daily at 02:30'`, `'weekly'`,
`'weekly on sunday at 03:00'`, or cron `'0 */6 * * *'`.

### `set_retention(max_backups int DEFAULT NULL, max_days int DEFAULT NULL, gfs text DEFAULT NULL) RETURNS text`
Who: admin. Whichever deletes first; newest always kept; `max_days` ≤ `pgbx.max_days_limit`. `gfs` (e.g.
`'7d,4w,12m'`, `y` = years) **also** keeps the newest backup of each of the last N days / ISO weeks / months / years
(UTC); a GFS keeper is never deleted by the other two rules; its span (12m = 372 days) must fit
`pgbx.max_days_limit`, else it is refused; `'off'` clears it. The old two-argument calls keep working.
See [GFS](../../guides/schedules-retention/#gfs-grandfather-father-son).
```sql
SELECT pgbx.set_retention(max_backups => 14, max_days => 90);
SELECT pgbx.set_retention(max_backups => 7, gfs => '7d,4w,12m');
```

### `pause(reason text DEFAULT NULL) RETURNS text` / `resume() RETURNS text`
Who: admin. Stops/starts automatic backups. Manual backups still work.

### `set_data_scope(include text[] DEFAULT NULL, exclude text[] DEFAULT NULL) RETURNS text`
Who: admin. No arguments = every row. See [Data scope](../../concepts/data-scope/).

### `set_verify_schedule(schedule text) RETURNS text`
Who: admin. Default `'weekly on sunday at 04:00'`; `'never'` or `'off'` disables.

## Jobs (admin)

### `backup_now() RETURNS bigint`
Queues a backup. Returns the `history` id. While a backup of this database is still queued, returns that one
instead (`pgbx.coalesce_manual`, on), so five calls cost one dump. A `NOTICE` says when it starts, how long it
takes and what limits it, e.g. `pgbx: backup job 42 queued, starts ~now, takes ~18 min (12 GB, medium confidence,
limited by upload (35 MB/s))`; on a busy server another one says it competes with the app (`pgbx.gate_manual_jobs`).

### `restore(into_db text, at timestamptz DEFAULT now(), with_roles bool DEFAULT false, roles text DEFAULT 'referenced') RETURNS bigint`
Restores the newest backup at or before `at` into a **new** database. Live database untouched. Raises the same
`NOTICE` as `backup_now()`; with `pgbx.restore_lane` it does not wait behind a running dump. An encrypted dump is
decrypted in the stream with `pgbx.encryption_key_file`; older unencrypted dumps restore as before.
`with_roles`: first create the roles in the backup's roles file that do **not** exist on this server (existing roles
are never changed), then restore keeping object owners; `roles`: `referenced` (the roles this database uses) or `all`.
See [Roles with every backup](../../guides/roles/).
```sql
SELECT pgbx.restore(into_db => 'myapp_restored', at => '2026-10-01 09:00');
SELECT pgbx.restore('myapp_restored', with_roles => true);
```

### `verify_now() RETURNS bigint`
Restores the newest backup into a scratch database, checks it, drops it. Coalesced like `backup_now()`.

### `cancel(job_id bigint) RETURNS text`
Cancels a job of this database. A queued one never starts. A running one is stopped by the worker within a few
seconds: its pg_dump / pg_restore is killed, a half-done upload is aborted (nothing is left in S3), a half-restored
database is dropped, and no alert is sent. Either way it ends as `cancelled`. Revoked from `PUBLIC`; granted to admin.
`pgbx jobs cancel ID --yes` does the same from the CLI.

### `pitr_backup_now() RETURNS bigint` — superuser, admin database
Queues a point-in-time base backup (kind `base_backup`) in the server-wide queue; refused while `pgbx.pitr` is off.
Cancel it like any job with `cancel(job_id)`. Not granted to anyone.

### `download_url(backup_id bigint DEFAULT NULL, expires interval DEFAULT '1 hour') RETURNS text`
Presigned S3 link to one backup (newest when `backup_id` is NULL). `expires` between 1 minute and 7 days. The link
serves the object as stored: an encrypted dump stays ciphertext (`pgbx decrypt`).
Revoked from `PUBLIC`; granted to admin.

