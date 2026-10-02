---
title: pgbx serve (web app)
description: A local web app served by the pgbx binary — backups, restore helper, read-only queries and health in a browser.
sidebar: { order: 4 }
---

`pgbx serve` opens a web app on your own machine for the server you point it at. It is the CLI with a
screen: same connection, same profiles, same safety rules.

```sh
pgbx serve --profile prod                 # opens http://127.0.0.1:<random port>/#token=...
pgbx serve --profile prod --allow-safe    # also lets you back up / verify / restore / cancel, each confirmed
```

The app is built into the binary. Nothing is fetched from the internet and nothing is reported anywhere.

## Screens

- **Overview**: every database with its state, last backup age and size, next run, backups kept and the
  last restore test. A failing database is shown in red with the reason. Under it: the job queue (running and
  waiting jobs with progress and ETA, from `pgbx jobs`), the load gate (`pgbx load`) and the quietest time to
  back up (from `pgbx schedule suggest`). The suggestion card only shows the `configure()` call and the CLI
  command to copy. It never applies them.
- **Database**: backups kept, the history timeline (backups, restores, restore tests, config changes,
  `cancelled` jobs), and the schedule, retention and data scope, read-only.
- **Restore helper**: pick a database, then the newest backup, one backup or a time. It shows the exact command,
  ready to copy:
  `pgbx db-restore --db shop --into shop_restore_20261002_1400 --time '2026-10-02 13:58:12+00'`.
  A restore only ever goes into a **new** database.
- **Query**: read-only SQL through the same guard as [`pgbx query`](../cli/#commands), with a result grid and
  CSV / JSON export. The database's saved questions (from its `memories.md`) are one click away. The
  app writes to memory only when you press **Save to memory**, which appends a `## name` section the way
  the agent skill does. It never rewrites the file.
- **Health**: the `pgbx doctor` checks, each with its fix. Advice rows (`schedule_in_quiet_window`,
  `eta_accuracy`) show as warnings and never make the server unhealthy.

On a server **without the pgbx extension** (client-only use) the overview says backups are off and how to turn
them on. The database list, queries and health still work.

## Flags

| flag | default | |
|---|---|---|
| `--profile NAME` | the default profile | the connection to open first; the header switches between all `pgbx profile list` entries |
| `--listen IP:PORT` | `127.0.0.1:0` | `0` = a random free port |
| `--no-open` | off | print the URL, do not open a browser |
| `--allow-safe` | off | turn on the safe-tier actions (below) |
| `--json` | off | print the start line as JSON: `{"ok", "url", "listen", "safety", "warnings"}` |

Connection flags (`--url`, `--host`, `--port`, `--user`, `--admin-db`, ...) work as for every command. A
profile with an [adapter](../../guides/adapters/) starts it on first use and keeps that one adapter running for
the whole `serve` run (switching away and back reuses it); Ctrl-C stops it. Passwords are masked in every API
answer.

## Safety

- **Loopback, random port, per-run token.** The link carries a token in its `#fragment`. The browser sends it
  in an `X-Pgbx-Token` header on every `/api/` call; a call without it gets `401`. The token is new each run.
- **Host check.** On a loopback address only `Host: localhost`, `127.0.0.1` or `[::1]` is answered (`403`
  otherwise). This stops DNS-rebinding pages, like [`pgbx ui`](../../guides/audit-ui/) does.
- **Reads are read-only.** Every database connection runs `SET default_transaction_read_only = on`. One
  connection per profile is kept open while `serve` runs.
- **Actions are off by default.** Without `--allow-safe`, every action call is refused with `403`. With it,
  only the safe tier is offered, each behind a confirm dialog that shows the equivalent CLI command:

  | button | runs |
  |---|---|
  | Back up now | `pgbx now --db X` |
  | Verify now | `pgbx verify --db X` |
  | Restore into new database | `pgbx db-restore --db X --into NEW [--time TS]` (refuses an existing database or the source) |
  | Cancel (queued job) | `pgbx jobs cancel ID --db X --yes`, only while the job is still **queued** |

  They go through the CLI's own code paths. Guarded and destructive actions (pause, lowering retention,
  narrowing scope, cancelling a running job, the load gate) are never offered. Use the CLI for those.
- A non-loopback `--listen` prints a warning. The token is still required, but prefer an SSH tunnel.

`pgbx ui` stays as it is: the read-only audit UI with the 30-day timeline across all databases. See the
[Audit UI guide](../../guides/audit-ui/).

## API

The app's JSON endpoints, for scripts that already hold the token:

| endpoint | returns |
|---|---|
| `GET /api/session` | version, profiles, `allow_safe`, enabled `actions` |
| `GET /api/overview` | `backups` (`on`/`off`), `connection`, `databases[]`, `windows[]` |
| `GET /api/queue`, `/api/load`, `/api/health` | as `pgbx jobs`, `pgbx load`, `pgbx doctor` |
| `GET /api/db/NAME`, `/api/suggest/NAME`, `/api/memory/NAME` | one database's detail, its quiet window, its saved questions |
| `POST /api/query` `{db, sql, max_rows}` | as `pgbx query` |
| `POST /api/memory/NAME` `{name, note, sql}` | appends one saved question |
| `POST /api/action/backup\|verify\|restore\|cancel` | only with `--allow-safe` |

Add `?profile=NAME` to use another profile. Agents should use the CLI's `--json` instead; they never need
`pgbx serve`.
