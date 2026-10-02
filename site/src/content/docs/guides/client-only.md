---
title: Use pgbx as your Postgres client, no backups needed
description: Profiles, SSH and jump hosts, read queries as JSON and agent memory against any Postgres, without installing the pgbx extension.
sidebar: { order: 0 }
---

You do not need backups to use pgbx. The CLI is a good everyday Postgres client on its own: it remembers your
servers, reaches them over SSH (jump hosts included), answers read queries as JSON, and gives your AI agent a
notebook per database. None of that needs the pgbx extension on the server, and nothing needs S3 or sudo.

## Install the CLI only

```sh
curl -fsSL https://deemwar-products.github.io/pgbx/install.sh | sh
```

On your laptop that puts `pgbx` on your PATH and installs the agent skill. Do not run `pgbx setup server`;
leave the database server alone.

## Save a server as a profile

```sh
pgbx setup client prod --host db.example.com --user app_ro          # reachable directly
pgbx setup client prod --ssh ops@db.example.com --user postgres     # only reachable over SSH
pgbx setup client prod --ssh ops@db.internal --ssh-jump bastion.example.com --user postgres   # through a jump host
```

`setup client` saves the profile (the first one becomes the default), connects once to test it, and reports:

```
ok: true
test:
  backups: off
  connected: true
  extension: absent
  info: pgbx extension not installed on this server: backups are off; queries, profiles and tunnels work
  postgres_version: 16.4
next_steps:
  - optional, to turn on backups: on the database server run `curl ... | sh` and then `sudo pgbx setup server`
```

`backups: off` is not an error, and the command exits 0. The next step is optional: ignore it until you want
backups.

Profiles hold where a server is, never a password: put passwords in `~/.pgpass` or `PGPASSWORD`. Manage them
with `pgbx profile list | show | use | remove` ([CLI reference](../../reference/cli/#profiles)).

## SSH and jump hosts

With `--ssh`, pgbx runs your own `ssh` program, so your keys, your agent, `~/.ssh/config` hosts and `-J` jump
hosts all work and pgbx never sees a key. The first command opens a tunnel in the background, later commands
reuse it, and it closes after 10 minutes unused (`--tunnel-idle 30m` to change that).

```sh
pgbx tunnel              # open it now and print the local port (point a GUI at 127.0.0.1:<port>)
pgbx tunnel list         # open tunnels
pgbx tunnel close prod   # close one (--all for every one)
```

Postgres on the far side is reached as the server sees it: `--host` is the address *from the SSH host*
(default `localhost`), so a database on another machine behind the bastion works too.

## Read queries as JSON

```sh
pgbx query "SELECT datname, pg_size_pretty(pg_database_size(datname)) AS size FROM pg_database"
pgbx query "SELECT state, count(*) FROM pg_stat_activity GROUP BY 1" --db shop --max-rows 200 --timeout 10s
pgbx query "SHOW max_connections" --profile staging
```

Every reply is one JSON object: `columns` with their types, `rows` with typed values, `row_count`,
`truncated`. `pgbx query` runs one SELECT-style statement inside a read-only transaction with a statement
timeout, and refuses writes and functions with side effects. That is a guard for agents, not a security
boundary: for a hard guarantee, connect as a role that can only read.

## Agent memory per database

Your agent keeps what it learns about each database in plain Markdown files on your machine:

```
~/pgbx/<profile>/<db>/memories.md   # facts and named questions with their SQL
~/pgbx/<profile>/<db>/tables.md     # tables, columns and what they mean
```

It reads them before answering and writes them only when you ask ("remember this", "save this query as
orders today"). Move them between machines with `pgbx memories export` and `pgbx memories import`; see
[For AI agents](../../agents/skill/#memory-one-folder-per-database).

## What works without the extension

| command | without the extension |
|---|---|
| `setup client`, `profile`, `tunnel`, `query`, `memories`, `skill` | works |
| `status` | exit 0: `backups: off` and the sentence above |
| `doctor` | exit 0: an `info` line for the extension; over SSH it checks through the tunnel when the server has no pgbx CLI |
| `now`, `verify`, `list`, `logs`, `overview`, `schedule`, `db-restore`, ... | refuse with "pgbx extension not installed on this server: backups are off ..." |
| `backups --from-s3`, `db-restore --from-s3` | work (they read S3 directly), if you have backups in a bucket |

## Turning backups on later

Nothing to migrate: run the [server install](../../getting-started/install/#on-the-database-server) on the
database server. Your profiles, tunnels and memory stay as they are, and `pgbx status` starts showing backups.
