---
title: SQL functions
description: Every public function and view in the pgbx schema.
sidebar: { order: 1 }
---

Per-database functions run inside the database they manage. `overview()` and `doctor()` run only in the admin
database (`pgbx.admin_db`, default `postgres`). Jobs are rows in `pgbx.history`
(`queued` → `running` → `done` | `failed` | `expired`).

## Status (viewer)

### `status()`
```sql
pgbx.status() RETURNS TABLE (
  database name, state text, schedule text, cron text, next_backup_at timestamptz,
  last_backup_at timestamptz, last_backup_age interval, last_backup_size text, last_backup_key text,
  backups_kept bigint, retention text, data_scope text, verify_schedule text, last_verified_at timestamptz,
  last_verify_result text, last_error text, last_error_at timestamptz, paused_reason text, paused_at timestamptz,
  queued_jobs bigint, location text)
```
Who: `pgbx_viewer`. One row: "are we backed up?"
```sql
SELECT state, last_backup_at, next_backup_at FROM pgbx.status();
```

### `backups` (view)
Columns: `id, taken_at, age, trigger, size, bytes, s3_key`. Every kept backup, newest first. Who: viewer.
```sql
SELECT * FROM pgbx.backups;
```

### `history` (table)
Every backup, restore, verify, config change, pause, prune. Who: viewer (read).
```sql
SELECT id, kind, state, error FROM pgbx.history ORDER BY id DESC LIMIT 10;
```

### `overview()` — admin database
```sql
pgbx.overview() RETURNS TABLE (database name, state text, schedule text, last_backup_at timestamptz,
  last_backup_age interval, last_backup_size text, next_backup_at timestamptz, backups_kept bigint,
  last_verify text, last_error text, seen_at timestamptz)
```
Who: viewer. One row per database on the server.

### `doctor()`
```sql
pgbx.doctor() RETURNS TABLE (name text, ok bool, detail text, fix text)
```
Who: viewer (runs as `SECURITY DEFINER`). Health checks: `extension loaded`, `s3 settings`, `credentials file`,
`database backups` (age vs schedule), `restore tests`, `workers`, `archive_mode` (info only), `replication_slots`
and `long_running_job` (a pg_dump / pg_restore running longer than `pgbx.doctor_long_job`).

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
                      enabled bool DEFAULT NULL, path text DEFAULT NULL) RETURNS pgbx.config
```
Who: `pgbx_admin`. Only the arguments you pass change. Made for migrations.
```sql
SELECT pgbx.configure(schedule => 'every 6 hours', max_backups => 28, max_days => 30);
SELECT pgbx.configure(enabled => false);   -- opt out (recorded)
```

### `set_schedule(schedule text) RETURNS text`
Who: admin. `'every 15 minutes'`, `'every 1 hour'`, `'hourly'`, `'daily'`, `'daily at 02:30'`, `'weekly'`,
`'weekly on sunday at 03:00'`, or cron `'0 */6 * * *'`.

### `set_retention(max_backups int DEFAULT NULL, max_days int DEFAULT NULL) RETURNS text`
Who: admin. Whichever deletes first; newest always kept; `max_days` ≤ `pgbx.max_days_limit`.
```sql
SELECT pgbx.set_retention(max_backups => 14, max_days => 90);
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
instead (`pgbx.coalesce_manual`, on), so five calls cost one dump.

### `restore(into_db text, at timestamptz DEFAULT now()) RETURNS bigint`
Restores the newest backup at or before `at` into a **new** database. Live database untouched.
```sql
SELECT pgbx.restore(into_db => 'myapp_restored', at => '2026-10-01 09:00');
```

### `verify_now() RETURNS bigint`
Restores the newest backup into a scratch database, checks it, drops it. Coalesced like `backup_now()`.

### `cancel(job_id bigint) RETURNS text`
Cancels a **queued** job of this database; it never starts and ends as `cancelled`. A running job cannot be
cancelled yet. Revoked from `PUBLIC`; granted to admin.

### `download_url(backup_id bigint DEFAULT NULL, expires interval DEFAULT '1 hour') RETURNS text`
Presigned S3 link to one backup (newest when `backup_id` is NULL). `expires` between 1 minute and 7 days.
Revoked from `PUBLIC`; granted to admin.

