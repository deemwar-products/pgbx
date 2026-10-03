---
title: Busy servers
description: "Keep backups out of your app's way: the job queue, time estimates, the quiet window and the load gate."
sidebar: { order: 16 }
---

pgbx runs on the database server, so its jobs compete with your app. Four things keep that in check. Every setting
named here is in [Settings](../../reference/settings/).

## One queue for the whole server

Every database's backups, restore tests, restores and prunes go through one server-wide queue, picked in this order:
restore > manual backup > scheduled backup > restore test > prune, then oldest first. By default one job runs at a time
(`pgbx.max_concurrent_jobs`), plus a separate slot so a restore never waits behind a long dump (`pgbx.restore_lane`).

```sh
pgbx jobs                    # what runs, what waits, and why
pgbx jobs cancel 42 --yes    # cancel a queued or running job (a running upload is aborted, nothing is left in S3)
```

A dump that takes longer than its schedule interval does not run back to back: the slots it overran are skipped
(`pgbx.overrun_policy`). pg_dump also never queues behind DDL: if a migration holds a table lock, the backup is retried
later instead of blocking it.

## Time estimates

`backup_now()`, `verify_now()` and `restore()` print a NOTICE with the expected start, duration, size and the
bottleneck (CPU, disk or network). While a job runs, `pgbx jobs`, `status()` and `pgbx ui` show its progress and
finish time. First estimates are deliberately pessimistic; they sharpen after a few jobs.

## Quiet window

pgbx learns how busy each hour of the week is and suggests the quietest one for your schedule. It never changes the
schedule by itself:

```sh
pgbx schedule suggest --db shop            # show the suggestion
pgbx schedule suggest --db shop --apply    # apply it (asks first)
```

`doctor()` warns (`schedule_in_quiet_window`) when your schedule sits in a much busier hour than the suggestion.

## Load gate

Before a scheduled backup or restore test starts, pgbx checks the load: active sessions, transactions per second,
long writing transactions, replica lag and load average. The default is **shadow**: it only records which jobs it
*would* have held back (`pgbx load` shows them), so you can see the effect before turning it on.

```sh
pgbx load                              # last sample, thresholds, would-defer counts
pgbx load --gate on --db shop --yes    # defer scheduled jobs of this database while the server is busy
```

With the gate on, a deferred job still runs at the latest `pgbx.max_defer` (4h) after it was queued, never later than
one schedule interval, so a busy server delays a backup but never skips it.

## Resource caps

pg_dump and pg_restore run niced (`pgbx.job_nice`) and, on Linux, at idle-ish IO priority (`pgbx.job_ionice`).
Upload and download bandwidth can be capped with `pgbx.upload_kbps` / `pgbx.download_kbps`.
