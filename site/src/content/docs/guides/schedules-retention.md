---
title: Schedules & retention
description: When backups run and how long they are kept.
sidebar: { order: 1 }
---

## Schedule

```sql
SELECT pgbx.set_schedule('every 1 hour');
SELECT pgbx.set_schedule('every 15 minutes');
SELECT pgbx.set_schedule('daily at 02:30');
SELECT pgbx.set_schedule('weekly on sunday at 03:00');
SELECT pgbx.set_schedule('0 */6 * * *');      -- plain cron works too
```

Also accepted: `'hourly'`, `'daily'`, `'weekly'`. Times are UTC. A bad schedule is rejected immediately.
Check the cron form of any text with `SELECT pgbx.to_cron('daily at 02:30');`.

```sh
pgbx schedule --db myapp                     # show
pgbx schedule 'every 6 hours' --db myapp     # set (safe)
```

## Retention

```sql
SELECT pgbx.set_retention(max_backups => 14, max_days => 90);
```

Whichever rule deletes first wins. The newest backup is always kept.
`max_days` may not exceed the server's `pgbx.max_days_limit` (default 90).

```sh
pgbx retention --db myapp                                    # show
pgbx retention --db myapp --max-backups 28 --max-days 30 --yes
```

Lowering retention deletes backups, so `pgbx` treats it as **guarded** and wants `--yes`.

## All at once, in a migration

```sql
SELECT pgbx.configure(schedule => 'every 6 hours', max_backups => 28, max_days => 30);
```

