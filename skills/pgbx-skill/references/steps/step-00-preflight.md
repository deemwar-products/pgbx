# Step 00 — Preflight

Run **once per session**, before any other `pgbx` call.

```bash
command -v pgbx && pgbx --version 2>/dev/null
```

Outcomes:

- **`pgbx` not on PATH** — respond exactly:
  > `pgbx` is not installed. It ships with pgbx v0.3 — from the pgbx repo run
  > `curl -fsSL https://deemwar-products.github.io/pgbx/install.sh | sh` (installs the binary and this skill; `pgbx skill install` re-installs just the skill).
  > Until then I can use the SQL API if you give me a `psql` connection.

  If `psql` works, set `pgbx_available=false` and use the `Fallback (SQL):` lines.
  Otherwise **stop**.

- **`pgbx` on PATH** — proceed. If `pgbx skill where --json` shows `same_version: false`, suggest `pgbx skill install`.

## Pick the server (profile)

```bash
pgbx profile list --json
```

- **One profile, or a `default` set** — use it; tell the user which (`"Using profile prod (db.prod:5432)."`).
- **Several profiles, no default, or the user named a server** — match the user's words to a profile name or
  `settings.host`; if still ambiguous, **ask** which one (list the names). Never guess between servers.
- **No profiles** — use the flags/env the user gave (`PGHOST` etc.); offer
  `pgbx profile add <name> --host H --port P --user U` (safe; never put a password or S3 key in it).
  A server reachable only over SSH: `pgbx profile add <name> --ssh user@host [--ssh-port N] [--ssh-jump J]`
  — pgbx starts the tunnel with the system ssh and reuses it; you never run `ssh` yourself.
  `pgbx tunnel list --json` shows open tunnels; `pgbx tunnel close <name>` closes one.

Set `profile` in session context and pass `--profile <name>` on **every** later `pgbx` call
(recipes show commands without it — add it). Check `profile_used` in each reply matches.

## Health

```bash
pgbx doctor --json
```
Fallback (SQL): `psql -XAtq -d postgres -c "SELECT row_to_json(d) FROM pgbx.doctor() d"` (pgbx v0.3)

Stop on unhealthy and give the plain-language fix, e.g.:

| doctor finding | say |
|---|---|
| extension not preloaded | "Add `pgbx` to `shared_preload_libraries` and restart Postgres." |
| S3 unreachable / auth failed | "Postgres can't write to the bucket. Check `pgbx.s3_endpoint`/`s3_bucket` and the credentials file (I won't read it)." |
| `archive_mode` info row | "pgbx does not need `archive_mode`; leave it unless nothing else uses it." (info only) |
| Postgres not reachable | "Postgres is down — only the CLI can help; see step-04 (or step-03 if the server is lost)." |

Read-only intents may continue past warnings; mutations may not continue past failures.
