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


## GFS (grandfather-father-son)

Keep long-term history without keeping every backup:

```sql
SELECT pgbx.set_retention(max_backups => 7, gfs => '7d,4w,12m');
SELECT pgbx.set_retention(gfs => 'off');      -- back to max_backups / max_days only
```

```sh
pgbx retention --db shop --gfs 7d,4w,12m       # adding GFS where there was none: no --yes needed
pgbx retention --db shop --gfs off --yes       # changing or clearing an existing GFS spec is guarded
```

`'7d,4w,12m'` additionally keeps the newest backup of each of the last 7 calendar days, last 4 ISO weeks
(Monday-based) and last 12 calendar months, in UTC; `y` = years. The rules combine:

- a backup is kept if `max_backups`/`max_days` keep it **or** GFS keeps it; a GFS keeper is never deleted
  by `max_backups` or `max_days`;
- the newest backup is always kept;
- the GFS span (12m = 372 days) must fit `pgbx.max_days_limit` (default 90): raise the limit first,
  otherwise `set_retention` refuses.

`status()` shows it (`max 7 backups, max 90 days, gfs 7d,4w,12m`). Each pruned dump's roles file is deleted with it.
