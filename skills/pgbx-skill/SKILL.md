---
name: pgbx-skill
description: "Zero-touch PostgreSQL backups to S3 via `pgbx` (pgbx): status, backup now, restore a database (also from S3 onto a new server), verify, schedule, retention. Trigger: is postgres backed up, take a backup, backup before deploy, restore the db, restore on a new server, backup failing, postgres is down, disk full, wal growing, pgbx."
---

<!-- version: 0.5.0 -->

# pgbx-skill

A natural-language front door for pgbx, the PostgreSQL extension that backs
every database up to S3 with no setup per database. `pgbx` is the engine; this
skill owns intent routing and recipe knowledge.

## Core Rules

- **`pgbx` is the engine.** Every recipe runs a `pgbx …` command first (requires
  pgbx v0.3). SQL against the `pgbx` schema is the documented fallback,
  used only when the CLI is not available. Recipes mark those lines `Fallback (SQL):`.
- **Always `--json`, never parse decorative output.** With SQL, use
  `psql -XAtq` and statements that return one row (`row_to_json(...)`). Tables,
  colours and `psql` borders are for humans, never for parsing.
- **Pick the server first, then pin it.** Run `pgbx profile list --json` (step-00), choose or ask
  for the profile, and pass `--profile <name>` on every `pgbx` call. Never mix servers in one task.
- **No shell needed: use the CLI.** Read questions → `pgbx query "SELECT ..." --json` (STAT-R-7); a remote
  server → a profile with `--ssh` (pgbx opens and reuses the tunnel itself). Prefer these over `ssh`, `psql`
  or any shell command; never try to get around `pgbx query`'s SELECT-only guard.
- **Discover before acting.** Run `pgbx status --json` / `pgbx doctor --json`
  (step-00) before any mutation. A `failing` database fails the next backup too.
- **Never print credentials.** S3 keys live in `pgbx.credentials_file`
  (or `sec`). Read setting NAMES only; never `cat` the file or echo a key.
  `download_url()` output is a bearer link: use it, never paste it into chat.
- **Four safety tiers** (full table in `references/workflow.md`):
  read-only → just do it; safe mutation → do it and report the id;
  higher-risk → ask first; destructive → explicit human approval, quoting
  exactly what will be replaced.
- **The guarded gate is enforced in code — never bypass it.**
  `--yes`, the admin-database check and superuser are the human's signature.
  Never add them on your own, never retry with them after the error, never escalate roles.
- **Restore one database = into a NEW database, never in place.** The live
  database is untouched; swapping names afterwards is destructive and needs approval.

## Session Context

Held in conversation memory only — no file writes.

```
pgbx_available:   true | false      # from step-00
profile:          prod               # from `pgbx profile list --json`; --profile on every call
target_db:        myapp             # database the user means
admin_db:         postgres          # pgbx.admin_db, for overview()/doctor()
last_job_id:      1234              # history id of the job we just queued
```

## Process

1. Read `references/router.xml` first — it routes the user's phrase to a route
   and lists the steps + family reference for it.
2. Run `references/steps/step-00-preflight.md` (pgbx on PATH, `pgbx profile list --json`, `pgbx doctor --json`).
3. Read `references/workflow.md` for invariants, the safety tiers and failure handling.
4. Follow the route's step file, if any:
   - Backup before deploy → `references/steps/step-01-backup-before-deploy.md`
   - Restore one database → `references/steps/step-02-restore-one-database.md`
   - Postgres down / disk full / pg_wal growing → `references/steps/step-04-postgres-down.md` (diagnose FIRST)
   - Server lost, restore on a new server → `references/steps/step-03-disaster-restore.md`
5. Load the matching family file and run the recipe verbatim.

## Self-check

    sh tests/all.sh

Runs:
  - tests/validate-recipes.sh   frontmatter, family files, recipe format, router refs
  - tests/install-test.sh        install/uninstall idempotency
  - tests/router-stress.sh       phrase → expected route

## Families at a glance

| Family | Covers | Reference |
|---|---|---|
| Status | is it backed up, list backups, doctor, logs, audit UI / who did what, read queries | `references/status.md` |
| Backup | backup now, wait, backup id | `references/backup.md` |
| Restore | db into a new database, from S3 onto a new server | `references/restore.md` |
| Verify | restore tests, verify schedule, failures | `references/verify.md` |
| Policy | schedule, retention, pause/resume, data scope | `references/policy.md` |
| Access | roles, download links | `references/access.md` |
| Diagnose | postgres down, disk full, WAL growing, OOM, corruption — cause + tiered steps | `references/diagnose.md` |
