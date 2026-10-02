---
title: How backups work
description: One logical backup per database, streamed to S3, restored into a new database.
sidebar: { order: 2 }
---

pgbx backs up **each database on its own**. There is no whole-server backup and no WAL archiving:
`archive_mode` is not needed and no extra package is installed.

| | |
|---|---|
| Engine | `pg_dump -Fc` streamed to S3 (16 MB multipart parts, no temp file) |
| Needs | `shared_preload_libraries = 'pgbx'` and the S3 settings |
| Schedule | per database, `set_schedule()` (default daily at 02:00) |
| Retention | `max_backups` / `max_days` per database (the newest backup is always kept) |
| Restore | `restore()` into a **new** database; the live one is never touched |
| Point in time | the newest backup taken at or before `at` |
| Restore tests | `verify_now()` / `set_verify_schedule()` restore into a scratch database, check, drop |
| Who | `pgbx_admin` (per database), superuser for server-wide settings |

## The worker

One background worker (`pgbx scheduler`) runs in every server that preloads the library. Each poll
(`pgbx.poll_seconds`) it:

1. installs (or updates) the extension in every database and in `template1`, so new databases are born with it;
2. queues scheduled backups and restore tests, and runs queued jobs oldest first;
3. expires old dumps (`max_backups`, `max_days`) and prunes the audit trail (`pgbx.audit_days`);
4. publishes each database's state to `server_overview` in the admin database (`overview()`, `doctor()`).

## Losing the whole server

Every dump is self-contained in S3. On a brand-new server — with or without pgbx installed —
`pgbx db-restore --from-s3` pulls a database back. See [Disaster recovery](../../guides/disaster-recovery/).
