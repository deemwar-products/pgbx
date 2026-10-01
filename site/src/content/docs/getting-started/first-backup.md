---
title: First backup in 60 seconds
description: Create a database, watch it get backed up, restore it.
sidebar: { order: 2 }
---

## 1. Create a database

```sql
CREATE DATABASE myapp;
```

Nothing else. A new database gets its first backup immediately, then follows its schedule
(default: daily at 02:00, keep 14 backups, max 90 days).

## 2. Watch it

```sql
\c myapp
SELECT state, last_backup_at, next_backup_at, location FROM pgbx.status();
SELECT * FROM pgbx.backups;     -- id, taken_at, age, trigger, size, bytes, s3_key
```

Or from the shell:

```sh
pgbx status --db myapp
pgbx list --db myapp --json
```

## 3. Take one now

```sql
SELECT pgbx.backup_now();       -- returns a job id
SELECT state, error FROM pgbx.history WHERE id = 42;
```

```sh
pgbx now --db myapp --wait
```

## 4. Restore it into a new name

```sql
SELECT pgbx.restore(into_db => 'myapp_restored');
```

The live database is never touched. Check `myapp_restored`, then swap names yourself.

## 5. Set your own policy (in a migration)

```sql
SELECT pgbx.configure(schedule => 'every 6 hours', max_backups => 28, max_days => 30);
```

Bad schedules and retention above `pgbx.max_days_limit` are rejected immediately.
