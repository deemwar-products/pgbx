---
name: pgbx-skill
---

# Workflow — invariants

Routing lives in `router.xml`. This file holds invariants only.

## Variables

- `{db}` — the database the user means (e.g. `myapp`).
- `{admin_db}` — `pgbx.admin_db`, default `postgres`. `overview()` and `doctor()` run only there.
- `{job}` — the `pgbx.history` id returned by `backup_now()` / `restore()` / `verify_now()`.
  Poll `history.state` (`queued` → `running` → `done` | `failed` | `expired`).

## Invariants

1. `pgbx … --json` first; SQL fallback only when pgbx is unavailable. Never parse decorative output.
2. Read-only discovery (`pgbx status --json`, `pgbx doctor --json`) before any mutation.
3. Never read or print credentials (`pgbx.credentials_file`, `docker/*.credentials`, `.env`).
4. One-database restore always lands in a NEW database.
5. Every job is waited on and its final `state` + `error` reported; never claim success from "queued".

## Safety tiers

| tier | actions | rule |
|---|---|---|
| read-only | `pgbx status/list/doctor/logs`, `pgbx profile list/show`, `status()`, `overview()`, `doctor()`, `pgbx backups --from-s3`, `pgbx.backups`, `pgbx.history`, `rowless_tables()` | no approval needed |
| safe mutation | `pgbx profile add/use/remove`, `pgbx now`, `pgbx verify`, `pgbx db-restore --into <NEW db>` (also `--from-s3`), `backup_now()`, `verify_now()`, `resume()`, `restore(into_db => <NEW db>)`, short-lived `download_url()` | allowed; report what you did and the id |
| higher-risk | `pause()` expected > 1 hour, lowering retention, narrowing data scope, `set_verify_schedule('never')`, long-lived or third-party `download_url()` | ask the human first; say what protection is lost |
| destructive | swapping/dropping the live database, dropping backups, disabling backups (`configure(enabled => false)`) | ALWAYS explicit human approval in this conversation, quoting exactly what will be replaced |

**The guarded gate is enforced in code.** `pgbx` refuses guarded changes without `--yes`, and every
restore refuses an existing database. A gate error is the system working:
go back to the human. Never add the flag yourself, never escalate privileges, never edit
`postgresql.conf` to get around it.

## Failure handling

| symptom | meaning | action |
|---|---|---|
| `pgbx: command not found` | CLI not installed | step-00 install text; SQL fallback if psql works |
| status `failing` | newest failure after newest success | quote `last_error`; run doctor; report; don't deploy on top of it |
| job `failed` | that job's `error` column says why | report verbatim; retry once only if transient (S3/network) |
| job stuck `queued` | background worker not running | `shared_preload_libraries` must include `pgbx` (restart) — tell the human |
| "… run in database …" (admin database) | wrong database | reconnect to `{admin_db}` |
| "permission denied" | role lacks `pgbx_viewer`/`_admin`/superuser | report the missing role; never grant it to yourself |
| verify `FAILED:` | newest backup does not restore | treat as NOT backed up (verify.md VFY-R-3) |
