---
title: Testing & guarantees
description: What the test suites prove, and the known limits.
---

All suites run against fresh containers. Check counts are **to be re-measured** for 0.5.0 (whole-server and
WAL suites were removed).

| suite | checks | proves |
|---|---|---|
| `tests/e2e.sh` | to be re-measured | auto-install in new databases, first backup, schedules, retention, pause/resume, data scope, roles, one-database restore, verify, download links, against a real S3 bucket |
| `tests/cli_e2e.sh` | to be re-measured | every `pgbx` command, `--json` shape, safety gates (`--yes`), `db-restore --from-s3` onto a plain Postgres container without the extension, Postgres-down paths, diagnose |
| `tests/ui_e2e.sh` | to be re-measured | `pgbx ui`: pages, JSON, GET only, viewer role; no whole-server endpoint |
| `tests/upgrade_e2e.sh` | to be re-measured | worker auto-update (`ALTER EXTENSION pgbx UPDATE`, incl. `template1`), reset of the old product's `archive_command` only, and (with `PGBX_OLD_IMAGE`) restoring the old product's dumps |
| `tests/diag_smoke.sh`, `tests/ui_smoke.sh` | to be re-measured | throwaway-container smokes: doctor/diagnose with pg_wal piling up; UI + audit retention |
| `tests/bench_local.sh` | to be re-measured | ~1.2 GB against a local S3: backup and restore throughput, S3 outages, fast shutdown |

`tests/run_all.sh` rebuilds, starts fresh containers, uses a unique S3 folder and runs the suites.
Set `PG_MAJOR=13`…`18` to run them on another Postgres major (needs a dev image built with that `PG_MAJOR`).
`cd cli && cargo test` covers the CLI's unit tests (argument parsing, safety levels, S3 key selection, diagnose classification).

## Robustness built in

- Streaming: `pg_dump` → 16 MB multipart parts → S3, no temp file. Restore streams S3 → `pg_restore`.
- Each part is retried with backoff (~2 min). A restore download resumes from the byte it reached.
- Half-finished uploads are aborted; orphaned ones are cleaned at startup.
- A Postgres shutdown never waits on a backup (stops within ~1 s; the job is marked interrupted).

## Known limits

- A restore is only as fine-grained as the database's schedule: you get the newest dump at or before the
  time you ask for. Back up more often (`every 15 minutes`) where that matters.
- No point-in-time restore between dumps (no WAL archiving since 0.5.0).
- Built and tested on PostgreSQL 16.
