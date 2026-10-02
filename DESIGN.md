# pgbx — per-database backups, zero touch

Zero-touch Postgres backups: **create a database and it is backed up**. Manual backup and restore are SQL calls.
The engine is `pg_dump`/`pg_restore` streamed to and from S3; pgbx is the control layer around it.
It does **per-database backups only** (no whole-server mode, no WAL archiving:
one model, no `archive_mode`, no extra package, and a restore never rewinds more than the database you asked for).

## Behaviour
- Installed in `template1`, so every `CREATE DATABASE` is born with schema `pgbx` and a default config.
- A background worker (started by Postgres via `shared_preload_libraries`) also scans `pg_database` every minute and
  installs the extension in any database that lacks it (created from `template0`, restored, pre-existing).
- A newly seen database gets its first backup immediately, then follows its schedule.
- Teams change settings in their normal migrations; nobody has to set anything up.

## Server-wide settings (postgresql.conf, reload with `SELECT pg_reload_conf();`)
```ini
shared_preload_libraries   = 'pgbx'
pgbx.s3_endpoint      = 'https://hel1.your-objectstorage.com'
pgbx.s3_bucket        = 'my-backups'
pgbx.s3_region        = 'hel1'
pgbx.server_name      = 'deemwar-db'                       # top folder for this server
pgbx.credentials_file = '/etc/pgbx/s3.credentials'  # root-owned, readable by postgres only
```
Keys never live in SQL tables.

## Per database (`pgbx.config`, one row) — only path + policy
| column | default |
|---|---|
| path | database name |
| schedule | `0 2 * * *` |
| retention_days | 14 |
| enabled | true |

## SQL
```sql
SELECT pgbx.configure(schedule => '0 */6 * * *', retention_days => 30);   -- optional, in a migration
SELECT pgbx.configure(enabled => false);                                  -- opt out (recorded)
SELECT pgbx.backup_now();                                                 -- returns job id
SELECT pgbx.restore(at => '2026-10-01 09:00', into_db => 'myapp_restored');
SELECT * FROM pgbx.history ORDER BY started DESC;
```

## Bucket layout
```
s3://<bucket>/<server_name>/
  <path>/<timestamp>.dump    per-database logical snapshots (restore one DB into a new name)
```

## Restore kinds
- **One database:** `pgbx.restore(...)` → restored into a new database name; live DB untouched; swap when verified.
- **Server lost:** `pgbx db-restore --from-s3` lists `<server>/<path>/` in S3, picks the newest dump at or before the
  requested time, creates the new database (refusing an existing one) and streams the dump into `pg_restore` with
  HTTP Range resume — on any server, with or without the extension.

## Weekly restore test
Restores the latest backup into a scratch database, runs integrity checks, writes the result to `history`,
alerts on failure. A backup that has never been restored is not a backup.

## Stack
Rust + pgrx (extension + background worker), PostgreSQL 13–18 for the extension (`pgbx db-restore --from-s3` needs only pg_restore on the restoring host and any server the dump can be restored into), pg_dump/pg_restore engine.
Developed and tested in Docker; production only after all checks pass.

## Done when
1. A new database is backed up within a minute with no action.
2. `backup_now()` works.
3. Drop a table → `restore()` into a new name → it is back.
4. Server-wide S3 settings, separate path per database.
