---
title: For AI agents
description: The pgbx agent skill, safety tiers and the JSON contract.
sidebar: { order: 1 }
---

One agent skill for Claude Code and Codex: `pgbx-skill`. It routes plain requests
("is postgres backed up?", "backup before deploy", "restore the db", "postgres is down") to `pgbx` commands.

## Install

```sh
pgbx skill install              # unpacks the skill embedded in the binary
pgbx skill install --no-codex   # Claude Code only
pgbx skill where                # where it is
pgbx skill uninstall
```

It unpacks to `~/.local/share/pgbx/skill/<version>/` and links `~/.claude/skills/pgbx-skill`
(`$CLAUDE_SKILLS_DIR`) and, when `$AGENTS_SKILLS_DIR` is set or `codex` is on PATH,
`~/.agents/skills/pgbx-skill`. The installer (`install.sh` / `install.ps1`) does this for you.

From a checkout: `sh skills/pgbx-skill/install.sh`; self-check: `sh skills/pgbx-skill/tests/all.sh`.

## Rules the skill follows

- `pgbx … --json` first. SQL is the fallback when the CLI is missing.
- Pick the server first: `pgbx profile list --json`, choose (or ask), then `--profile NAME` on every call.
- No shell: read questions go through `pgbx query "SELECT ..." --json`; a server behind SSH is a profile with
  `--ssh`, and pgbx opens and reuses the tunnel. The skill never runs `ssh` or `psql` itself and never tries to
  get around the query guard.
- Discover (`pgbx status --json`, `pgbx doctor --json`) before any change.
- Never read or print credentials. A `download_url` link is used, never pasted into chat.
- One-database restore always lands in a **new** database.
- Every job is waited on; success is never claimed from `queued`.

## Memory: one folder per database

The agent keeps what it should know about each database in plain Markdown files on your machine:

```
~/pgbx/<connection>/<db>/memories.md   # facts and named questions with their SQL ("orders today")
~/pgbx/<connection>/<db>/tables.md     # tables, columns and what they mean
```

`<connection>` is the profile name (`prod`), `<db>` the database. Before it writes a query or acts on a
database, the agent reads both files, so "how many orders came in today?" reuses the SQL you saved instead of
guessing table names. It **never creates or changes these files on its own**: only when you say "remember
this", "put it here", "save this query as …" or "note the tables". It never stores rows, passwords, keys or
download links. They are your files: edit them by hand, keep them in git, or delete them.

| setting | default | |
|---|---|---|
| `PGBX_MEMORY_DIR` | `~/pgbx` | where the per-database folders live |
| `PGBX_MEMORY` | `on` | `off`: the agent neither reads nor writes memory |

Move memory between machines or connections:

```sh
pgbx memories export --profile prod                 # -> pgbx-memories-prod.json (one database: --db shop)
pgbx memories import pgbx-memories-prod.json        # on the other machine
pgbx memories import pgbx-memories-prod.json --as staging   # under another connection name
```

Import never replaces a file you edited locally: it lists it under `conflicts` and keeps yours, unless you add
`--overwrite`. `pgbx memories path` prints where a connection's memory lives.

## Safety tiers

| tier | examples | agent rule |
|---|---|---|
| read-only | `pgbx status/list/doctor/logs`, `status()`, `overview()` | just do it |
| safe | `pgbx now`, `pgbx verify`, `pgbx db-restore --into NEW` (also `--from-s3`), `resume()` | do it, report the job id |
| higher-risk | long `pause`, lowering retention, narrowing scope, `verify-schedule never` | ask the human first |
| destructive | swapping/dropping the live database, `configure(enabled => false)` | explicit human approval, quoting what will be replaced |

The guarded gate is enforced in code. `--yes` is the human's signature. An agent never adds it on its own,
never retries with them after the error, and never escalates roles.

## JSON contract

Every command with `--json` prints one object with `ok`, `command` and `safety`, and exits non-zero on failure.
Full shapes: [pgbx CLI reference](../../reference/cli/).

```sh
pgbx now --db myapp --wait --json
```

```json
{"ok": true, "command": "now", "safety": "safe", "database": "myapp", "job_id": 42,
 "state": "done", "watch": "SELECT * FROM pgbx.history WHERE id = 42", "job": {}}
```
