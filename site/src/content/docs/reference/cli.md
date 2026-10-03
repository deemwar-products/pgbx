---
title: pgbx CLI
description: Every pgbx command, its flags, JSON output and safety level.
sidebar: { order: 3 }
---

The pgbx CLI is the client: it talks to one or more Postgres servers (see [How it works](../../concepts/how-it-works/)).
With Postgres up, `pgbx` calls the pgbx extension's SQL. With Postgres down, `pgbx diagnose` explains why, and
`pgbx backups --from-s3` / `pgbx db-restore --from-s3` read dumps straight from S3 (no extension needed).

## Common flags

| flag | default |
|---|---|
| `--json` | one JSON object on stdout |
| `--profile NAME` | a saved server (see [Profiles](#profiles)); also `PGBX_PROFILE`; else the default profile |
| `--url URL` | a connection string for this run, no profile (`$VAR`s expanded); also `PGBX_URL` |
| `--db X` | the database to act on (default: the one in the connection string, else `postgres`) |
| `--host` / `--port` / `--user` | direct, no profile: `/var/run/postgresql`, `5432`, `postgres` (`PGHOST`, `PGPORT`, `PGUSER`, `PGPASSWORD` honoured) |
| `--admin-db` | `postgres` |
| `--timeout SECS` | for `--wait` |

Which connection a command uses: `--url` > `--profile` > `--host`/`--port` > `PGBX_URL` > `PGBX_PROFILE` > the
default profile > `PGHOST`/`PGPORT`/`PGUSER` and the defaults above. A connection string without a password
uses `PGPASSWORD`. Output never shows a password (`postgres://user:***@...`).

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
| `doctor` | readonly | `healthy`, `postgres_up`, `checks[]` (`name, ok, detail, fix`); `diagnosis` when Postgres is down (not through an adapter: an `info` row says to run it on the host) |
| `diagnose [--log F] [--pgdata DIR]` | readonly | `postgres`, `probable_cause`, `evidence[]`, `steps[]`, `facts`; on the database host only (refused with an adapter profile) |
| `ui [--listen 127.0.0.1:8432] [--strict]` | readonly | serves the read-only [audit UI](../../guides/audit-ui/) (with a point-in-time restore card) and Prometheus `GET /metrics`; `GET` only |
| `serve [--listen 127.0.0.1:0] [--no-open] [--allow-safe]` | readonly / safe (`--allow-safe`) | the local [web app](../serve/): overview, database detail, restore helper, read-only query, health; per-run token; safe actions only with `--allow-safe`; start line: `url`, `listen`, `warnings` |
| `metrics` | readonly | `text`: one Prometheus scrape (text mode prints just the metrics); see [Notifications and metrics](../../guides/notifications/) |
| `decrypt --key-file F [--in FILE] [--out FILE]` | readonly | decrypts an encrypted dump (e.g. from a download link); stdin → stdout by default (then the summary goes to stderr); `bytes`, `out` |
| `logs [--lines N]` | readonly | `recent_failures[]` |
| `jobs` | readonly | `jobs[]` (`database, job_id, kind, trigger, state, position, slot, detail, progress, eta_start, eta_finish, est_bytes, done_bytes`), `slots`; text mode: one line per job |
| `jobs cancel ID [--db X] --yes` | guarded | `database`, `job_id`, `message`; `--db` may be left out when the id is unique in the queue |
| `load [--db X]` | readonly | `sample` (last load sample: `load_busy`, `load_reasons`, `load_active`, `load_tps`), `settings` (gate and thresholds), `databases[]` (`load_gate, would_defer_7d, deferred_7d, forced_7d`), `deferred_jobs[]`; with `--db`: `recent_jobs` |
| `load --gate off\|shadow\|on\|default --db X [--yes]` | safe / guarded (`on`) | `database`, `load_gate` |
| `schedule suggest [--db X] [--hours N] [--apply [--yes]]` | readonly / safe (`--apply`) | `suggestion` (row of `suggest_window()`), `apply_sql`, `applied`; never applied unless asked: `--apply` asks y/N on a terminal, else needs `--yes` |
| `now [--db X] [--wait]` | safe | `database`, `job_id`, `state`, `watch`; with `--wait`: `job` |
| `verify [--db X] [--wait]` | safe | same as `now` |
| `db-restore --db X --into NEWDB [--time TS] [--with-roles [--roles referenced\|all]] [--wait]` | safe | same as `now`; refuses an existing database or the source; `--with-roles` creates the missing roles from the backup's roles file first and keeps owners |
| `db-restore --from-s3 --db X --into NEWDB [--backup KEY \| --time TS] [--with-roles [--roles R]] [--key-file F] <s3 flags>` | safe | `restored_into`, `key`, `bytes`, `encrypted`, `roles` (`created, existing, out_of_scope, skipped, failed`); works without the extension on the target; `--key-file` = the source's `pgbx.encryption_key_file` for encrypted dumps |
| `resume` | safe | `database`, `message` |
| `link [--backup-id N] [--expires '1 hour']` | safe | `database`, `url`, `expires` (text mode prints only the URL) |
| `schedule [TEXT]` | readonly / safe | `database`, `schedule` |
| `retention [--max-backups N] [--max-days N] [--gfs 7d,4w,12m\|off] [--yes]` | readonly / guarded | `database`, `max_backups`, `max_days`, `gfs`; lowering, or changing / clearing an existing GFS spec, needs `--yes` (adding GFS where there was none does not) |
| `pause --reason T --yes` | guarded | `database`, `message` |
| `scope [--include P1,P2] [--exclude P1,P2] [--reset] [--yes]` | readonly / guarded | `database`, `data_scope` |
| `verify-schedule TEXT\|never [--yes]` | safe / guarded (`never`) | `database`, result |
| `setup pitr [--yes]` (alias `setup --pitr`) | guarded | without `--yes`: `plan`, `current`; with it: `written`, `restart_needed`, `next`. ALTER SYSTEM `archive_mode = on`, `archive_command = '<this pgbx> wal-push %p'`, `pgbx.pitr = on`; refuses an `archive_command` pgbx did not write; needs a superuser; restart Postgres once |
| `skill install [--no-codex] \| uninstall \| where` | safe | `version`, `installed_to`, `files` / `removed`, `skipped` |
| `query "SQL" [--db D] [--max-rows 1000] [--timeout 30s]` | readonly | `columns[]` (`name, type`), `rows[]`, `row_count`, `truncated`, `database`, `user` |
| `memories export [FILE\|-] [--db D]` | readonly | `file`, `connection`, `databases[]`, `files`; one JSON bundle of `~/pgbx/<connection>/<db>/{memories,tables}.md` |
| `memories import FILE [--as C] [--overwrite]` | safe | `written[]`, `unchanged[]`, `conflicts[]` (differing local files are kept unless `--overwrite`) |
| `memories path` | readonly | `root`, `connection`, `dir` |
| `profile add NAME --url URL \| --adapter A [--adapter-command CMD] [key=value ...]` | safe | `profile` (`name, default, connection, adapter, runs, settings`), `replaced`, `file`, `notices` |
| `profile edit NAME [key=value \| key= \| --url \| --adapter \| --s3-...]` | safe | same as `add` |
| `profile list` / `profile show NAME` | readonly | `default`, `profiles[]`, `adapters`, `secrets` / `profile` (passwords masked) |
| `profile remove NAME` / `profile use NAME` | safe | `removed` / `default` |
| `--version` | — | `{"ok": true, "version": "..."}` |

## Point-in-time restore (optional, 0.6.0)

| command | safety | what |
|---|---|---|
| `pitr status` | readonly | `pitr` (row of `pitr_status()`) |
| `pitr list (--conf F \| <s3 flags> [--system-id N])` | readonly | `system_id`, `restorable_from`, `base_backups[]`, `gaps[]`, read straight from S3 |
| `pitr backup-now [--wait]` | safe | `job_id`; queues a base backup (superuser); with `--wait`: `job` |
| `pitr restore --time TS\|latest --target DIR (--conf F \| <s3 flags> [--system-id N])` | safe | `base_backup`, `data_directory`, `mode`, `start_command`, `restore_conf`, `bytes`, `mb_per_s`; restores into an **empty** directory and never starts Postgres; refuses a time inside a WAL gap |
| `… --yes-replace-whole-server` | destructive | `--target` is a **stopped** server's data directory; it is moved aside to `<dir>.pgbx-replaced-<time>` (not deleted); refused while Postgres runs |
| `wal-push %p [--conf F]` | — | `archive_command`: exit 0 = archived (or deliberately dropped under `pgbx.wal_queue_max`), otherwise Postgres retries |
| `wal-get %f %p --conf F` | — | `restore_command`: 0 delivered (verified), 1 not in the archive, 127 hard error |
| `pitr backup [--expire] / expire / publish-gaps --conf F` | — | run by the worker (a base backup job runs `pitr backup --expire --json`) |

`--conf` is `<pgbx.work_dir>/pgbx-wal.conf` (written by the worker; S3 settings and the credentials file's **path**,
never keys). With the server gone, use the S3 flags below instead. See
[Point-in-time restore](../../guides/point-in-time-restore/).

## S3 flags (`--from-s3`)

| flag | meaning |
|---|---|
| `--s3-endpoint URL` | S3 endpoint |
| `--s3-bucket B` | bucket |
| `--s3-region R` | region (default `us-east-1`) |
| `--server-name S` | the old server's folder (`pgbx.server_name`) |
| `--credentials-file F` | `access_key_id=` / `secret_access_key=` lines; never printed |

`db-restore --from-s3` creates `--into` (refusing if it exists) on the server of the current connection
(profile, `--url` or `--host/--port/--user`), then streams the dump into `pg_restore --no-owner` (owners kept with
`--with-roles`), resuming downloads with HTTP Range; `--key-file` decrypts an encrypted dump in the stream (plain
dumps pass through). `pg_restore` gets the password through its environment, never its arguments.
Example: [Disaster recovery](../../guides/disaster-recovery/).

## Profiles

A profile is a named connection, so you do not retype it and the S3 flags. It is either a connection string or
an adapter (a program that hands pgbx a connection string: SSH, AWS, GCP, Azure or your own; see
[the adapters guide](../../guides/adapters/)). All of it lives in `config.yaml`: see
[Configuration](../config/).

```sh
pgbx profile add prod --url 'postgres://ops:$PGPASSWORD@db.prod.example.com:5432/shop' \
  --s3-endpoint https://s3.eu-central-1.amazonaws.com --s3-bucket my-backups --s3-region eu-central-1 \
  --server-name db-prod-1 --credentials-file ~/.config/pgbx/prod.credentials
pgbx profile add bastion --adapter ssh target=ops@bastion.example.com pg_host=10.0.3.7 user=app 'password=$PGPASSWORD'
pgbx profile add corp --adapter corp --adapter-command '/usr/local/bin/corp-pg' env=prod   # your own adapter
pgbx profile edit bastion pg_port=6432 jump=     # set one setting, remove another
pgbx profile use prod            # the default when no --profile is given (the first profile added is the default)
pgbx status --profile bastion --db shop
PGBX_PROFILE=bastion pgbx list --db shop
pgbx profile list --json
pgbx profile remove corp
```

- `--url URL` or `--adapter NAME` plus `key=value` settings for that adapter (free-form: the adapter decides
  what it reads). `--host/--port/--user` on `add` are a shortcut for a `--url`. pgbx's own keys: `admin-db`,
  `s3-endpoint`, `s3-bucket`, `s3-region`, `server-name`, `credentials-file` (each also a flag) and
  `ready_timeout` (`ready_timeout=60s`).
- `--adapter-command CMD` defines the adapter in `config.yaml` and the reply says exactly what it will run.
  Without it, `--adapter ssh` (or `aws`, `gcp`, `azure`) registers the example the installer copied to
  `~/.config/pgbx/adapters` (`PGBX_ADAPTERS_DIR` overrides).
- **Never stores a secret.** Write `$VAR` references (single quotes in the shell); pgbx expands them when a
  command runs, from the environment, then the [secrets source](../config/#secrets). A literal password in a
  url, or in a setting named like `password`, `secret` or `token`, is refused. `show`/`list` mask passwords.
- Chosen by `--profile NAME`, else `PGBX_PROFILE`, else the default (see the order under
  [Common flags](#common-flags)). Every `--json` reply then carries `profile_used`. An unknown name is an error,
  never a silent fallback.
- An adapter starts on the command's first connection and stops when the command ends; Ctrl-C stops it too.
  `pgbx serve` keeps one per connection for its whole run.
- With an adapter profile, `diagnose` and `setup server` refuse and say to run them on the database host;
  `doctor` runs its SQL checks through the adapter.
- A `profiles.json` from pgbx 0.5 is moved into `config.yaml` on first use (SSH profiles become `adapter: ssh`);
  the old file is kept as `profiles.json.migrated`.

The built-in SSH of 0.5 (`--ssh`, `--ssh-port`, `--ssh-jump`, `--tunnel-idle`, `pgbx tunnel`) is gone; those
flags now fail with a pointer to the ssh adapter.

## `pgbx query`

```sh
pgbx query "SELECT datname, pg_database_size(datname) AS bytes FROM pg_database" --json
```

Checks, in order: exactly **one** statement; it starts with `SELECT`, `WITH`, `TABLE`, `VALUES`, `SHOW` or
`EXPLAIN` (not `EXPLAIN ANALYZE`); no `INSERT`/`UPDATE`/`DELETE`/`MERGE` anywhere (so no data-modifying
`WITH`), no `SELECT ... INTO`, no `FOR UPDATE`/`FOR SHARE`; no known side-effect functions
(`pg_terminate_backend`, `pg_cancel_backend`, `pg_reload_conf`, `set_config`, `nextval`, `setval`,
`pg_advisory*lock*`, `lo_*`, `dblink*`, `pg_switch_wal`, `txid_current`, `pg_sleep`, ...), and no `pgbx.*`
function except the read ones (`status`, `overview`, `doctor`, `rowless_tables`, `to_cron`, `next_run_epoch`).
Then it runs inside `BEGIN READ ONLY` with `SET LOCAL statement_timeout` (`--timeout`, default 30s) and
`lock_timeout` (at most 5s), and always `ROLLBACK`s.

Rows: numbers, booleans and nulls typed, JSON nested, everything else text (`SHOW`/`EXPLAIN`: all text);
at most `--max-rows` (default 1000), `truncated: true` when there were more.

:::caution
This is a **best-effort guard for agents, not a security boundary**. A word-level check can be fooled, and
`READ ONLY` does not stop every function with side effects. For a hard guarantee connect as a role that can
only read (for example one granted `pg_read_all_data`); creating it is up to you, pgbx never creates roles.
A keyword used as a quoted column name (`"update"`) is refused too.
:::
