---
title: How it works
description: The three parts of pgbx — the pgbx extension inside Postgres, the pgbx CLI that talks to your servers, and the agent skill on top — and what runs where.
sidebar: { order: 1 }
---

pgbx has three parts. You only *need* the first one; the other two make it easier to drive.

```
 laptop / CI / agent machine
   agent skill  ── runs ──▶  pgbx CLI  (profiles: prod, staging, ...)
                                │
                                │ normal Postgres connection (SQL)
                                ▼
 database server
   PostgreSQL
     └─ pgbx extension ── schedules, pg_dump ──▶  S3 bucket
                                                     ▲
   pgbx CLI, when Postgres is down or the server     │
   is gone: reads dumps straight from S3 ────────────┘
```

## The pgbx extension — runs inside Postgres

It is installed on the database server and loaded by Postgres itself. It does all the real work:

- notices every database and backs it up on a schedule (`pg_dump` to your S3 bucket),
- keeps only as many backups as the retention says,
- regularly restores the newest backup into a scratch database to prove it works,
- restores into a **new** database when you ask (`SELECT pgbx.restore(...)`).

Because it runs inside Postgres, backups keep happening when nobody is logged in and no CLI is open.
Everything it does is plain SQL, so any Postgres client (psql, your app, a GUI) can use it.

## The pgbx CLI — the client

`pgbx` is a single program you run on your laptop, in CI, or on the server. It connects to one or more
Postgres servers like any client and calls the extension's SQL for you: `pgbx status`, `pgbx now`,
`pgbx db-restore`, and so on. Every command takes `--json` and prints exactly one JSON object.

If you work with several servers, save each one as a **profile** and pick it with `--profile`:

```sh
pgbx profile add prod --host db.prod.example.com --user ops
pgbx status --profile prod --db shop
```

Profiles store where a server is, never passwords or S3 keys (use `~/.pgpass` / `PGPASSWORD`, and a
credentials file for S3). See [CLI reference](../../reference/cli/#profiles).

### Servers you reach over SSH

Many database servers only accept connections from inside their network. Add `--ssh` to the profile and the
CLI goes through SSH for you, using your normal `ssh` program (your keys, agent, `~/.ssh/config` and jump
hosts all work; pgbx never sees a key):

```sh
pgbx profile add prod --ssh ops@db.prod.example.com --user postgres
pgbx status --profile prod --db shop      # first command opens the tunnel
pgbx query "SELECT now()" --profile prod  # later commands reuse it
```

```
 pgbx CLI ──▶ 127.0.0.1:<free port> ══ ssh -L ══▶ db server ──▶ Postgres (localhost:5432)
               └─ kept open by a small background pgbx helper; closes after 10 minutes unused
```

Commands that need the machine itself (`doctor`, `logs`, `diagnose`, `setup`) run there as
`ssh ops@db... pgbx <command> --json`, so the server needs the pgbx CLI too (the install one-liner puts it
there). `pgbx tunnel list` shows open tunnels; `pgbx tunnel close prod` closes one.

### Looking around without a shell

`pgbx query "SELECT ..."` runs one read query and returns typed JSON rows, so you (or an agent) can answer
"how big is this database?" without psql or a login on the box. It only accepts SELECT-style statements and
refuses writes and functions with side effects. This is a **best-effort guard for agents, not a security
boundary**: if you need a hard guarantee, connect as a role that can only read. Roles and permissions stay
yours to manage; pgbx never creates one.

## The agent skill — on top of the CLI

`pgbx-skill` teaches AI coding agents (Claude Code, Codex) to answer "is my database backed up?" or
"restore yesterday's data" by running `pgbx ... --json` commands. It never talks to Postgres on its own,
so it gets the same safety checks the CLI enforces (risky actions need `--yes` from a human).
See [For AI agents](../../agents/skill/).

## What runs where

| part | runs on | needs Postgres up? |
|---|---|---|
| pgbx extension | the database server, inside Postgres | yes (it *is* part of Postgres) |
| pgbx CLI | anywhere: laptop, CI, the server (also over SSH) | for most commands; not for the ones below |
| agent skill | wherever the agent runs | no, it only calls the CLI |

## When Postgres is down

The extension stops with Postgres, but your backups are already in S3. The CLI still helps:

- `pgbx diagnose` (on the server) reads the logs and disk to say why Postgres is down.
- `pgbx backups --from-s3` lists the dumps straight from the bucket.
- `pgbx db-restore --from-s3` restores a dump onto **any** Postgres, even a brand-new server without the
  extension. See [Postgres is down](../../guides/postgres-down/) and
  [Disaster recovery](../../guides/disaster-recovery/).
