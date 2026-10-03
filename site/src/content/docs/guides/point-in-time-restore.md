---
title: Point-in-time restore (optional)
description: Archive WAL and take base backups so the whole server can be restored to any moment. Off by default.
sidebar: { order: 12 }
---

Per-database dumps stay the default: they restore one database into a new name, as of a dump.
Point-in-time restore (PITR) is an **optional, whole-server** layer on top: pgbx archives every WAL file to S3
(`pgbx wal-push` as `archive_command`) and takes scheduled base backups (`pg_basebackup`), so a server can be
rebuilt as of any second inside the retention window. It is a small engine inside pgbx: no pgBackRest is installed
or run.

## Enable

Set up the server first (`pgbx setup server`, see [Install](../../getting-started/install/)), then:

```bash
pgbx setup pitr          # shows the plan (guarded)
pgbx setup pitr --yes    # ALTER SYSTEM: archive_mode=on, archive_command='<abs path>/pgbx wal-push %p', pgbx.pitr=on
# restart Postgres ONCE (archive_mode needs it); the first base backup is queued right after
```

`setup pitr` (also spelled `setup --pitr`) needs a superuser. It **refuses** to replace an `archive_command` that
pgbx did not write (another tool archives WAL there); remove that yourself first if it is truly unused. A non-default
`pgbx.work_dir` is passed to wal-push as `--conf`. `pgbx setup server` never touches `archive_mode`: per-database
backups do not need it.

## Settings

| setting | default | what |
|---|---|---|
| `pgbx.pitr` | `off` | turn PITR on (with archive_mode=on and the pgbx archive_command) |
| `pgbx.pitr_schedule` | `daily at 01:00` | when base backups are queued (same forms as `set_schedule()`) |
| `pgbx.pitr_retention` | `7 days` | restore window: every base backup needed to reach any moment of the last N days is kept (including the newest one that stopped before the cutoff), the newest backup is always kept, older backups and WAL before the oldest kept backup's start are deleted (history files kept) |
| `pgbx.wal_queue_max` | `4GB` | when WAL waiting to be archived exceeds this (S3 down), wal-push **drops** WAL instead of filling the disk; each drop is logged and recorded as a **gap**; `off` = never drop |
| `pgbx.wal_gap_margin` | `60s` | restores are also refused this long before a gap's last safe moment |
| `pgbx.wal_alert_after` | `15 min` | alert when the oldest WAL waiting to be archived is older than this |
| `pgbx.wal_alert_size` | `2GB` | alert when this much WAL is waiting |
| `pgbx.work_dir` | `<data_directory>/../pgbx` | conf (`pgbx-wal.conf`, 0600, no keys: only the credentials file path), spool, drop log; outside the data directory |
| `pgbx.cli_path` | `/usr/local/bin/pgbx` or `/usr/bin/pgbx` | the CLI the worker runs for base backups |

## Base backups are jobs

A base backup is an ordinary job of the [server-wide queue](../../reference/settings/#job-queue): kind
`base_backup` in the admin database, with the priority of a scheduled backup. It takes a job slot and runs
`pgbx pitr backup --expire --json --conf <work_dir>/pgbx-wal.conf` as its child (`pg_basebackup -Ft -X none` →
sha256 → multi-threaded zstd → parallel multipart upload, then `backup.json`, then expiry). So:

- `pgbx jobs` shows it running or waiting, with the reason;
- `SELECT pgbx.cancel(id)` (in the admin database) or `pgbx jobs cancel ID --yes` stops it: the child is killed and
  the half-written upload never becomes a base backup (no `backup.json`);
- a Postgres shutdown stops it promptly;
- never two base backups at once.

Take one now (superuser): `SELECT pgbx.pitr_backup_now();` or `pgbx pitr backup-now --wait`.

## Gaps and alerts

The worker watches archiving every poll (it never waits for S3 itself):

- **Archiving stuck or failing** (oldest waiting WAL older than `pgbx.wal_alert_after`, or more than
  `pgbx.wal_alert_size` waiting): one `wal_archive` incident, an alert when it opens, a reminder every hour, and one
  "recovered" alert only when WAL really reached S3 again and nothing was dropped recently.
- **WAL dropped** under `pgbx.wal_queue_max`: a `wal_gap` row and an alert. A gap closes only when a base backup that
  **started after the last drop** finishes; pgbx queues that healing backup by itself once WAL reaches S3 again (with
  backoff after failures).

Alerts go to `pgbx.alert_command` and to the [notification channels](../notifications/) (`pgbx.notify`), sent from a
thread of their own so a slow webhook never holds up the worker. Base backup failures alert like any failed job.

## S3 layout

```
s3://<bucket>/<server_name>/<system_id>/
  base/<label>/base.tar.zst   pg_basebackup -Ft -X none, zstd, parallel multipart
  base/<label>/backup.json    start/stop LSN and time, timeline, sizes, sha256 (written last)
  wal/<timeline>/<file>.zst   segments, .history, .backup; x-amz-meta-sha256 of the raw file
  gaps.json                   recorded gaps (so restores with Postgres down can refuse them)
```

wal-push is idempotent: the same file with the same checksum is success; the same name with a different checksum
is an error and is never overwritten (two servers archiving into one path). wal-get verifies every file it delivers.

## Status

```sql
SELECT * FROM pgbx.pitr_status();   -- admin database; viewers may call it
```

`enabled, state, last_base_backup_at, restorable_from, wal_archived_until, open_gaps, gaps, backlog_segments,
backlog_bytes, last_error, location`. CLI: `pgbx pitr status`, `pgbx pitr list` (straight from S3).
`pgbx doctor` adds rows `pitr archiving`, `pitr base backups`, `pitr gaps`; `pgbx ui` shows a PITR card.

## Restore

```bash
pgbx pitr restore --time '2026-10-01 09:00:00+00' --target /srv/pg-restored \
     --conf /var/lib/postgresql/pgbx/pgbx-wal.conf
# then run the printed command, e.g.
pg_ctl -D /srv/pg-restored -o '-p 5433' -l /srv/pg-restored/pgbx-restore/recovery.log start
```

- Picks the newest base backup that stopped at or before the time, downloads it in parallel ranges and verifies its
  sha256, writes `restore_command = 'pgbx wal-get %f %p --conf …'`, `recovery_target_time`,
  `recovery_target_action = 'promote'` and `recovery.signal`. `--time latest` replays everything archived.
- **pgbx never starts Postgres**; it prints the exact start command.
- A time inside a recorded gap (with the margin) is refused.
- A **copy** (empty or new directory) gets `archive_mode = off`, `pgbx.pitr = off` and
  `pgbx.server_name = '<server>-pitr-copy-<label>'`, so it never archives into or backs up over the original.
- **In place** is destructive: `--yes-replace-whole-server` with `--target` = a STOPPED server's data directory;
  it is moved aside to `<dir>.pgbx-replaced-<time>` (not deleted). Refused while Postgres runs.
- **Server gone / Postgres down / no extension**: replace `--conf` with
  `--s3-endpoint U --s3-bucket B --server-name S --credentials-file F [--system-id N]`.

## Performance design

Designs ported from pgBackRest (MIT; attribution in `NOTICE`): async archive-push with a spool, where a background
process pushes ahead of Postgres in parallel; archive-get prefetch of the next segments in parallel during replay;
the archive-push-queue-max drop semantics; parallel multipart uploads and ranged downloads for base backups and
restores; time-based expiry. zstd level 1 for WAL, multi-threaded level 3 for base backups.

## Measured

`tests/pitr_bench.sh`, PostgreSQL 16, one Apple M5 Pro (18 CPUs in the container), S3 = RustFS on the same
machine (so the network is never the limit: expect less from a remote bucket), 2026-10-02. pgBackRest 2.59.2 ran in
the same container against the same S3, for comparison only.

| what | pgbx | pgBackRest |
|---|---|---|
| `wal-push`, one 16 MB segment, sync (200 calls) | p50 51–53 ms, p99 58–65 ms | p50 77–79 ms, p99 85–89 ms |
| archiving a backlog of 121 segments (1.9 GB) left by `pgbench -i -s 150` + 30 s of 8 clients, async, 4 processes (2 runs) | 55.0 segments/s (880 MB/s), both runs | 52.6 and 43.2 segments/s |
| `wal-get` replay, 200 sequential calls, no prefetch | 19–23 segments/s | 13.5–14.4 segments/s |
| `wal-get` replay with prefetch (pgbx 16 segments; pgBackRest async) | 54–57 segments/s | 22.7–25.0 segments/s |
| base backup of a ~2 GB server (1.83–2.17 GB; 1.92–2.27 GB tar → 673–706 MB zstd) | 5.1–5.5 s | 4.3–5.0 s (full backup) |
| restore of it (download + sha256 + unpack) | 2.4–2.5 s | 2.6–3.0 s (files only) |
| then WAL replay to `latest` + promote, all rows present | 2.1–2.4 s | — |
| the same on a 4.1 GB server (4.31 GB tar → 793 MB): backup / restore / replay | 14.1 s / 4.4 s / 4.9 s | 8.1 s / 8.2 s / — |

Not measured here: archiving while the load runs is bounded by how fast Postgres writes WAL (this machine wrote
121 segments in ~45 s), so the drain of a backlog is what shows the push side's ceiling.

## Limits (v1)

- Only the default tablespaces (a server with extra tablespaces is refused; per-database dumps still cover it).
- Base backups and WAL are zstd + sha256 but **not** encrypted by pgbx (`pgbx.encryption_key_file` covers
  per-database dumps and their roles files only); rely on S3 server-side encryption and bucket access control.
- The prefetch spool and restore conf live in `<target>/pgbx-restore/`.
- PostgreSQL 13–18.
