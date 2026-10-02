---
title: How it works
description: The three parts of pgbx — the pgbx extension inside Postgres, the pgbx CLI that talks to your servers, and the agent skill on top — and what runs where.
sidebar: { order: 1 }
---

pgbx has three parts. For backups you only *need* the first one; the other two make it easier to drive.
The CLI also works on its own, against any Postgres without the extension: see
[Use pgbx as your Postgres client](../../guides/client-only/).

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
pgbx profile add prod --url 'postgres://ops:$PGPASSWORD@db.prod.example.com:5432/shop'
pgbx status --profile prod
```

Profiles live in one file, `~/.config/pgbx/config.yaml`, and hold `$VAR` references, never passwords or S3
keys: pgbx reads `$PGPASSWORD` from your environment (or your secrets source) when a command runs. S3 keys stay
in a credentials file. See [Configuration](../../reference/config/).

### Servers behind SSH or a cloud: adapters

Many database servers only accept connections from inside their network, or through cloud tooling. pgbx has
no connection code of its own for that. A profile names an **adapter** instead: any program that hands pgbx a
connection string. Examples ship for SSH, AWS (SSM), GCP (Cloud SQL Auth Proxy) and Azure, and you can write
your own in any language:

```sh
pgbx profile add prod --adapter ssh target=ops@db.prod.example.com user=postgres 'password=$PGPASSWORD'
pgbx status --profile prod                  # starts the adapter, uses it, stops it
pgbx query "SELECT now()" --profile prod    # the next command starts it again
```

```
 pgbx CLI ── start (stdin) ──▶ adapter (e.g. node ssh-adapter.js)
          ◀── {"url": ..., "state": "ready"} (one stdout line)
                                 └─ ssh -N -L 127.0.0.1:<free port> ══▶ db server ──▶ Postgres
 pgbx CLI ── SQL to the url ──▶ 127.0.0.1:<free port>
 pgbx CLI ── stop ──▶ adapter exits; pgbx kills what is left of its process group
```

The URL stays in pgbx's memory and every output masks its password. `pgbx serve` keeps one adapter running
for its whole run. See [Connect through SSH, AWS, GCP, Azure or your own adapter](../../guides/adapters/).

Commands that need the machine itself (`diagnose`, `setup server`) run on the database host: with an adapter
profile pgbx says so instead of guessing. `doctor` runs its SQL checks through the adapter and leaves the disk
and log checks to a `pgbx doctor` on the host.

### Looking around without a shell

`pgbx query "SELECT ..."` runs one read query and returns typed JSON rows, so you (or an agent) can answer
"how big is this database?" without psql or a login on the box. It only accepts SELECT-style statements and
refuses writes and functions with side effects. This is a **best-effort guard for agents, not a security
boundary**: if you need a hard guarantee, connect as a role that can only read. Roles and permissions stay
yours to manage; pgbx never creates one.

### In a browser

`pgbx serve --profile prod` opens a local web app on your machine: every database's backups at a glance, the
job queue, a restore helper that writes the exact `pgbx db-restore` command, read-only queries and the health
checks. It is the same CLI behind a screen, on 127.0.0.1 with a per-run token, read-only unless you start it
with `--allow-safe`. See [pgbx serve](../../reference/serve/).

## The agent skill — on top of the CLI

`pgbx-skill` teaches AI coding agents (Claude Code, Codex) to answer "is my database backed up?" or
"restore yesterday's data" by running `pgbx ... --json` commands. It never talks to Postgres on its own,
so it gets the same safety checks the CLI enforces (risky actions need `--yes` from a human).
See [For AI agents](../../agents/skill/).

## What runs where

| part | runs on | needs Postgres up? |
|---|---|---|
| pgbx extension | the database server, inside Postgres | yes (it *is* part of Postgres) |
| pgbx CLI | anywhere: laptop, CI, the server (reaching Postgres directly or through an adapter) | for most commands; not for the ones below |
| agent skill | wherever the agent runs | no, it only calls the CLI |

## When Postgres is down

The extension stops with Postgres, but your backups are already in S3. The CLI still helps:

- `pgbx diagnose` (on the server) reads the logs and disk to say why Postgres is down.
- `pgbx backups --from-s3` lists the dumps straight from the bucket.
- `pgbx db-restore --from-s3` restores a dump onto **any** Postgres, even a brand-new server without the
  extension. See [Postgres is down](../../guides/postgres-down/) and
  [Disaster recovery](../../guides/disaster-recovery/).
