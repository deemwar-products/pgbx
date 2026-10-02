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
| `serve [--listen 127.0.0.1:0] [--no-open] [--allow-safe]` | readonly / safe (`--allow-safe`) | the local [web app](../serve/): overview, database detail, restore helper, read-only query, health; per-run token; safe actions only with `--allow-safe`; start line: `url`, `listen`, `warnings` |
| `logs [--lines N]` | readonly | `recent_failures[]` |
| `jobs` | readonly | `jobs[]` (`database, job_id, kind, trigger, state, position, slot, detail, progress, eta_start, eta_finish, est_bytes, done_bytes`), `slots`; text mode: one line per job |
| `jobs cancel ID [--db X] --yes` | guarded | `database`, `job_id`, `message`; `--db` may be left out when the id is unique in the queue |
| `load [--db X]` | readonly | `sample` (last load sample: `load_busy`, `load_reasons`, `load_active`, `load_tps`), `settings` (gate and thresholds), `databases[]` (`load_gate, would_defer_7d, deferred_7d, forced_7d`), `deferred_jobs[]`; with `--db`: `recent_jobs` |
| `load --gate off\|shadow\|on\|default --db X [--yes]` | safe / guarded (`on`) | `database`, `load_gate` |
| `schedule suggest [--db X] [--hours N] [--apply [--yes]]` | readonly / safe (`--apply`) | `suggestion` (row of `suggest_window()`), `apply_sql`, `applied`; never applied unless asked: `--apply` asks y/N on a terminal, else needs `--yes` |
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
| `query "SQL" [--db D] [--max-rows 1000] [--timeout 30s]` | readonly | `columns[]` (`name, type`), `rows[]`, `row_count`, `truncated`, `database`, `user` |
| `memories export [FILE\|-] [--db D]` | readonly | `file`, `connection`, `databases[]`, `files`; one JSON bundle of `~/pgbx/<connection>/<db>/{memories,tables}.md` |
| `memories import FILE [--as C] [--overwrite]` | safe | `written[]`, `unchanged[]`, `conflicts[]` (differing local files are kept unless `--overwrite`) |
| `memories path` | readonly | `root`, `connection`, `dir` |
| `tunnel [open]` / `tunnel list` / `tunnel close NAME\|--all` | readonly | `local_port`, `connect`, `tunnel` / `tunnels[]` / `closed[]` |
| `profile add NAME [flags]` | safe | `profile` (`name, default, settings`), `replaced`, `file` |
| `profile list` / `profile show NAME` | readonly | `default`, `profiles[]` / `profile` |
| `profile remove NAME` / `profile use NAME` | safe | `removed` / `default` |
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

## Profiles

A profile is a named server, so you do not retype `--host/--port/--user` and the S3 flags:

```sh
pgbx profile add prod --host db.prod.example.com --port 5432 --user ops --admin-db postgres \
  --s3-endpoint https://s3.eu-central-1.amazonaws.com --s3-bucket my-backups --s3-region eu-central-1 \
  --server-name db-prod-1 --credentials-file ~/.config/pgbx/prod.credentials
pgbx profile add local --host localhost
pgbx profile use prod            # the default when no --profile is given (the first profile added is the default)
pgbx status --profile local --db shop
PGBX_PROFILE=local pgbx list --db shop
pgbx profile list --json
pgbx profile remove local
```

- Fields: `host`, `port`, `user`, `admin-db`, `s3-endpoint`, `s3-bucket`, `s3-region`, `server-name`,
  `credentials-file`, `ssh`, `ssh-port`, `ssh-jump`, `tunnel-idle` (each is also a flag). `add` on an existing
  name replaces it.
- Precedence for each value: **flag > environment** (`PGHOST`, `PGPORT`, `PGUSER`) **> profile > built-in
  default**. With no profile saved and none asked for, behaviour is unchanged.
- Chosen by `--profile NAME`, else `PGBX_PROFILE`, else the default. Every `--json` reply then carries
  `profile_used`. An unknown name is an error, never a silent fallback.
- Stored in `~/.config/pgbx/profiles.json` (`$XDG_CONFIG_HOME/pgbx`; `%APPDATA%\pgbx` on Windows;
  `PGBX_CONFIG_DIR` overrides), mode 0600.
- **Never stores passwords or S3 keys.** Passwords: `~/.pgpass` or `PGPASSWORD`. S3 keys: the
  `--credentials-file` (only its path is saved).

## SSH (`--ssh`)

| flag / profile field | meaning |
|---|---|
| `--ssh user@host` | reach the server through SSH (a `~/.ssh/config` Host name works too) |
| `--ssh-port N`, `--ssh-jump J` | `ssh -p`, `ssh -J` |
| `--tunnel-idle 10m` | close the shared tunnel after this long unused (`30s`, `10m`, `1h`) |
| `--host`, `--port` | where Postgres listens **as seen from the SSH host** (default `localhost:5432`) |

- Uses the system `ssh` (OpenSSH, also on Windows) with `BatchMode=yes`: keys, agent, `~/.ssh/config` and
  ProxyJump are ssh's; pgbx never handles a key or a password prompt.
- Postgres commands go through `ssh -N -o ExitOnForwardFailure=yes -o ServerAliveInterval=30 -L
  127.0.0.1:<free port>:<pg host>:<pg port>`, owned by a detached pgbx helper. Later commands for the same
  profile reuse it (the helper is alive and the port answers) and refresh its idle timer; otherwise a new one
  starts. A lock file stops two commands starting two. State: `~/.cache/pgbx/tunnels/<profile>.json`
  (`$XDG_CACHE_HOME`; `%LOCALAPPDATA%\pgbx\tunnels` on Windows; `PGBX_STATE_DIR` overrides), mode 0600.
- `pgbx tunnel` opens (or reuses) the tunnel and prints the local port, for psql or a GUI.
  `pgbx tunnel list` / `close NAME` / `close --all`.
- `doctor`, `logs`, `diagnose` and `setup` run on the host: `ssh target pgbx <command> ... --json` (setup as
  `sudo -n`), output and exit code relayed, plus `remote`. If pgbx is not installed there, the error gives the
  install one-liner.
- `PGBX_SSH` names another ssh binary or wrapper (the test suite uses it to add `-F config`).

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
