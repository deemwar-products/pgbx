---
title: Restore one database
description: Bring one database back into a new name. The live one is never touched.
sidebar: { order: 4 }
---

```sql
\c myapp
SELECT pgbx.restore(into_db => 'myapp_restored');                         -- newest backup
SELECT pgbx.restore(into_db => 'myapp_restored', at => '2026-10-01 09:00'); -- newest at or before
SELECT state, error FROM pgbx.history WHERE id = 43;
```

`restore()` returns a job id. It restores the newest per-database backup taken at or before `at`
into a **new** database. Restore streams S3 straight into `pg_restore`; a download resumes from the byte it reached.

```sh
pgbx db-restore --db myapp --into myapp_restored --wait
pgbx db-restore --db myapp --into myapp_restored --time '2026-10-01 09:00+00' --wait
```

`db-restore` refuses an existing database or the source database. `--time` must carry a UTC offset.

## Swap when verified

Swapping is up to you and is destructive:

```sql
-- after checking myapp_restored, with no connections to either
ALTER DATABASE myapp RENAME TO myapp_old;
ALTER DATABASE myapp_restored RENAME TO myapp;
```

A restore is only as fine-grained as your schedule: back up more often where that matters.
Server gone? Restore from S3 onto a new one with [`pgbx db-restore --from-s3`](../disaster-recovery/).
