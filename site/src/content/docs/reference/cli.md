---
title: pgbx CLI
description: Every pgbx command, its flags, JSON output and safety level.
sidebar: { order: 3 }
---

With Postgres up, `pgbx` calls the extension's SQL. With Postgres down, `pgbx diagnose` explains why, and
`pgbx backups --from-s3` / `pgbx db-restore --from-s3` read dumps straight from S3 (no extension needed).

## Common flags

| flag | default |
|---|---|
| `--json` | one JSON object on stdout |
| `--db X` | the database to act on |
| `--host` / `--port` / `--user` | `/var/run/postgresql`, `5432`, `postgres` (`PGHOST`, `PGPORT`, `PGUSER`, `PGPASSWORD` honoured) |
| `--admin-db` | `postgres` |
| `--timeout SECS` | for `--wait` |

`--time` must carry a UTC offset (`+00`, `Z`). Policy commands show the current value when given no arguments.
Exit code is non-zero on failure.

## JSON contract

Every `--json` reply is one object with at least:

```json
{"ok": true, "command": "now", "safety": "safe"}
```

`safety` is `readonly`, `safe`, `guarded` or `destructive`. On error: `{"ok": false, "error": "...", ...}`.

## Commands

| command | safety | output (besides ok/command/safety) |
|---|---|---|
| `status [--db X]` | readonly | `postgres`, `database`, `status` (row of `status()`) |
| `list [--db X]` | readonly | `database`, `database_backups[]` (`id, taken_at, age, trigger, size, s3_key`) |
| `backups --from-s3 --db X <s3 flags>` | readonly | `prefix`, `backups[]` (`key, taken_at, bytes`), newest first |
| `overview` | readonly | `databases[]` (rows of `overview()`) |
| `doctor` | readonly | `healthy`, `postgres_up`, `checks[]` (`name, ok, detail, fix`); `diagnosis` when Postgres is down |
| `diagnose [--log F] [--pgdata DIR]` | readonly | `postgres`, `probable_cause`, `evidence[]`, `steps[]`, `facts` |
| `ui [--listen 127.0.0.1:8432] [--strict]` | readonly | serves the read-only [audit UI](../../guides/audit-ui/); `GET` only |
| `logs [--lines N]` | readonly | `recent_failures[]` |
| `now [--db X] [--wait]` | safe | `database`, `job_id`, `state`, `watch`; with `--wait`: `job` |
| `verify [--db X] [--wait]` | safe | same as `now` |
| `db-restore --db X --into NEWDB [--time TS] [--wait]` | safe | same as `now`; refuses an existing database or the source |
| `db-restore --from-s3 --db X --into NEWDB [--backup KEY \| --time TS] <s3 flags>` | safe | `restored_into`, `key`, `bytes`; works without the extension on the target |
| `resume` | safe | `database`, `message` |
| `link [--backup-id N] [--expires '1 hour']` | safe | `database`, `url`, `expires` (text mode prints only the URL) |
| `schedule [TEXT]` | readonly / safe | `database`, `schedule` |
| `retention [--max-backups N] [--max-days N] [--yes]` | readonly / guarded | `database`, `max_backups`, `max_days` |
| `pause --reason T --yes` | guarded | `database`, `message` |
| `scope [--include P1,P2] [--exclude P1,P2] [--reset] [--yes]` | readonly / guarded | `database`, `data_scope` |
| `verify-schedule TEXT\|never [--yes]` | safe / guarded (`never`) | `database`, result |
| `skill install [--no-codex] \| uninstall \| where` | safe | `version`, `installed_to`, `files` / `removed`, `skipped` |
| `--version` | — | `{"ok": true, "version": "..."}` |

## S3 flags (`--from-s3`)

| flag | meaning |
|---|---|
| `--s3-endpoint URL` | S3 endpoint |
| `--s3-bucket B` | bucket |
| `--s3-region R` | region (default `us-east-1`) |
| `--server-name S` | the old server's folder (`pgbx.server_name`) |
| `--credentials-file F` | `access_key_id=` / `secret_access_key=` lines; never printed |

`db-restore --from-s3` creates `--into` (refusing if it exists) on the server given by `--host/--port/--user`,
then streams the dump into `pg_restore --no-owner`, resuming downloads with HTTP Range.
Example: [Disaster recovery](../../guides/disaster-recovery/).
