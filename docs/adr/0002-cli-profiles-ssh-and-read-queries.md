# 0002 — CLI profiles, SSH tunnels and read-only queries

- Status: accepted
- Date: 2026-10-02
- Branch: cli-profiles

## Context

pgbx has three parts: the **pgbx extension** (inside Postgres; schedules, `pg_dump` to S3, restore tests,
restores), the **pgbx CLI** (the client for people and agents) and the **agent skill** (drives the CLI with
`--json`). The goal is to make it fully agentic: an agent that has only the pgbx CLI must be able to reach
and inspect any of the user's servers without a shell. Until now the CLI knew about one server at a time,
set with `--host/--port/--user` or `PGHOST` and friends.

## Decisions

### 1. Named profiles, never secrets
`pgbx profile add|list|show|remove|use`, `--profile NAME` / `PGBX_PROFILE` on every command.
Precedence: **flag > env (PGHOST/PGPORT/PGUSER) > profile > built-in default**. With no profile, behaviour is
the same as before. Stored as JSON (`profiles.json`, mode 0600) in the OS config dir. It holds locations
only. Passwords stay in `~/.pgpass` / `PGPASSWORD`, and S3 keys stay in the credentials file, which the
profile names only by path. JSON was chosen over TOML so we need no new dependency (serde_json is already used).

### 2. SSH through the system `ssh`, with one tunnel per profile that gets reused
Profile fields `ssh`, `ssh-port`, `ssh-jump`, `tunnel-idle`. pgbx never handles keys: it runs the system `ssh`
with `BatchMode=yes`, so `~/.ssh/config`, the ssh agent and ProxyJump all work, Windows OpenSSH included.
Windows OpenSSH has no ControlMaster, so tunnel reuse is pgbx's own: a detached helper
(`pgbx tunnel --serve`) owns `ssh -N -o ExitOnForwardFailure=yes -o ServerAliveInterval=30 -L 127.0.0.1:<free>:…`
and records `{pid, ssh_pid, port, started, last_used, idle_secs, spec}` in the user cache dir (0600). Later commands
reuse it when the helper is alive and the port answers, and update `last_used`. The helper exits, and kills
ssh, after `tunnel-idle` (default 10m) unused, when ssh dies, or when `pgbx tunnel close` removes its state
file. A lock file stops two concurrent commands starting two tunnels. A state file whose `spec` differs
(another host or port) is replaced.
Host-side commands (`doctor`, `logs`, `diagnose`, `setup`) run as `ssh target pgbx <cmd> --json` and their
reply is passed through unchanged. `setup` runs under `sudo -n`, so it never waits on a password prompt.

### 3. `pgbx query`: SELECT-style only, a layered best-effort guard
pgbx is an admin tool, and roles and permissions belong to the user, so pgbx creates no role. (An earlier
plan for `setup --reader` / `pgbx_reader` / `query_user` was dropped.) Instead `pgbx query` applies four layers:
(a) exactly one statement; (b) it starts with SELECT/WITH/TABLE/VALUES/SHOW/EXPLAIN (no ANALYZE), has no
INSERT/UPDATE/DELETE/MERGE anywhere, no SELECT INTO and no FOR UPDATE/SHARE; (c) it calls no function on the
deny-list of side-effect functions, and no pgbx.* function other than the read-only ones (status, overview,
doctor, rowless_tables, to_cron, next_run_epoch); (d) it runs inside `BEGIN READ ONLY` with `SET LOCAL
statement_timeout` and `lock_timeout`, then ROLLBACK. Output: `columns[{name,type}]`, typed `rows`,
`row_count`, `truncated`.
**This is a guard for agents, not a security boundary.** The docs say so. A user who needs a hard guarantee
connects as a role that can only read.

### 4. The skill goes through the CLI only
The skill runs `pgbx profile list --json` first, passes `--profile` on every call, uses `pgbx query --json`
for read questions, and never runs `ssh`/`psql` itself.

## Consequences

- Agents can inspect remote servers with one binary and no shell access.
- The word-level checks refuse some harmless queries, for example a quoted column named `"update"`.
- The tunnel helper is a background process. It cleans up after itself, and `pgbx tunnel list|close` shows and stops it.
- Tests: unit tests (precedence, single-statement check, read-only guard, expiry with an injected clock,
  state files and the lock), `tests/ssh_e2e.sh` (docker sshd + plain postgres: create, reuse, idle expiry,
  recreate, query refusals), and `tests/cli_e2e.sh` (profiles and query against the extension).
