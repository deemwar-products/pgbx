# ADR 0003: Connection adapters and profiles (and a marketplace note)

Status: **Accepted in principle (owner decisions, 2026-10-02); build after the current merges.**
Builds on ADR 0002 (profiles, SSH tunnels, read queries).

## Context

Today a profile reaches Postgres directly (`--host/--port`) or over SSH (`cli/src/tunnel.rs`: a detached helper
owns `ssh -N -L ...`; it's reused across commands and closes after 10 idle minutes). Real databases often sit
behind cloud tooling: AWS (SSM Session Manager port forwarding, RDS IAM auth), GCP (`cloud-sql-proxy`) and Azure
(`az` / Bastion). Or they sit behind a company's own script. Users also have **many** of them: several AWS
accounts, GCP projects and environments.

## Decision

### Profiles are the core

- Full management for many profiles: `pgbx profile add | edit | remove | list | show | use`.
- A profile saves **only**: `name`, `adapter`, and that adapter's args or command, plus non-secret pgbx settings
  (db, user name, s3 bucket location, ...). **Never a URL, password, token or key.**
- Users with a single database can skip profiles entirely and pass a connection string for that run
  (`--url postgres://...` or `PGBX_URL`). It's used in memory and never saved.

### Adapters

An adapter is a program pgbx runs to get a connection. pgbx **re-runs the adapter for every command and stops it
when the command ends**. There's no helper daemon and no socket for adapters.

**Contract (v1).** pgbx runs the command with no shell, in its own process group (`setsid` on Unix, a Job Object
on Windows). The adapter prints **one JSON line** on stdout within `ready_timeout` (default 30 s):

```json
{"url": "postgres://user:pass@127.0.0.1:54321/shop?sslmode=require", "state": "ready", "name": "prod-eu"}
```

| Field | Meaning |
|---|---|
| `url` | full connection string, password included if needed. pgbx uses it **in memory for this run only** and saves it nowhere (not in profiles, state files, memories or logs). Any output redacts it to `postgres://user:***@...`. Child tools (`pg_dump`, `pg_restore`) get the password through their environment, never their argv, so it can't show up in `ps`. |
| `state` | shown to the user as is: `ready` means go; anything else (`error: no credentials for project acme-prod`, `mfa required`) means pgbx stops, shows it and exits non-zero. |
| `name` | the connection's name. It must match the profile name when run from a profile (a mismatch is an error, to catch a wrong adapter). With an ad-hoc adapter it becomes the connection name for memory (`~/pgbx/<name>/<db>/`). |

When the command ends, or on a ready-timeout or Ctrl-C, pgbx sends **SIGTERM to the adapter's process group**,
then **SIGKILL** after 5 s (on Windows it terminates the Job Object), so no proxy is left behind.

**pgbx does nothing with secrets.** Credentials belong to the adapter, the vendor CLI or the user. pgbx only
forwards and uses the URL.

### Exactly four built-in adapters

| Adapter | Wraps (the user's existing tooling and credentials) |
|---|---|
| `ssh` | system `ssh` with keys, agent, `~/.ssh/config` and ProxyJump (today's path) |
| `aws` | `aws ssm start-session` port forwarding to RDS or EC2, plus `aws rds generate-db-auth-token` for IAM auth when asked |
| `gcp` | `cloud-sql-proxy` (and `gcloud` for the instance connection name / IAM) |
| `azure` | `az` (Bastion tunnel and Entra ID token for Azure Database for PostgreSQL) |

No kubectl or other built-ins. Each built-in is a thin wrapper that runs the vendor CLI the user already has,
picks a free local port, waits until it accepts, and emits the same `{url, state, name}` line. A missing
vendor CLI gives `state: "error: aws CLI not found (install it, then aws configure)"`.

### Custom adapters: config only, no code from us

```toml
[additional_adapters]
corp-vpn = ["node", "corp-proxy.js"]
vault-db = ["/usr/local/bin/vault-pg", "--role", "readonly"]
```

`pgbx profile add billing --adapter corp-vpn --arg env=prod`. We embed no custom adapter code: we run the
command and read its line. The contract above is the whole API.

### Safety

- An adapter runs arbitrary code, so the profiles/config file is as sensitive as `~/.ssh/config`: mode 0600,
  `profile add` with a custom adapter prints exactly what it will run, and **`pgbx memories import` never
  carries profiles or adapters**.
- The agent skill may use a profile, but adds or edits one only when the user asks (guarded tier).
- `pgbx query` stays SELECT-only whatever the adapter.

### Prior art

| Mechanism | What we take | What we avoid |
|---|---|---|
| git credential helpers | exec a named helper, line protocol on stdout, stays out of the core | key=value text: we use one JSON line |
| kubectl exec credential plugins | versioned JSON contract, explicit env | a long-lived cache of the secret |
| Terraform `external` data source | program in, one JSON object out, non-zero = error | (no lifecycle there; we must stop the process) |
| ssh `ProxyCommand` | the user's own command decides the route | stdio-only transport: pg tools need a port |

## Consequences

- One code path for SSH and every cloud, and profiles become the product's centre for the client side.
- Per-command adapters are simple and leave nothing running, but each command pays the adapter's start-up
  (seconds for SSM or cloud-sql-proxy). Acceptable for backups and occasional queries; slow for rapid
  agent loops (Q1 below).
- Host-side commands (`doctor`, `logs`, `setup server`, `diagnose`) still need `ssh`; for cloud adapters
  they report that honestly.
- Tests: a fake adapter in e2e. It prints ready, then sleeps; prints an error state; never prints; prints a
  name that doesn't match; ignores SIGTERM (so SIGKILL must land). The URL must never appear on disk or in
  output.

## Marketplace (long term)

**The pitch (owner agreed):** "own your own Postgres backups." RDS/Cloud SQL snapshots restore only inside that provider. They can
be copied across regions and accounts, but not restored on another cloud or on a laptop, and the RDS export to
S3 is Parquet, not a restorable dump. pgbx dumps are plain `pg_dump` files in *your* bucket, with *your*
retention,
restorable anywhere with `pg_restore` or `pgbx db-restore --from-s3`. (Check these provider facts against
current docs before using them in marketing.)

**The catch that decides the shape:** managed Postgres (RDS, Aurora, Cloud SQL, Azure Flexible Server) does
**not** let you install custom extensions, so the pgbx *extension* cannot run there. A marketplace product for
managed Postgres needs a **runner outside the database**: the pgbx CLI on a schedule, running `pg_dump` over
the network into the customer's bucket, using its own scheduling and history instead of the in-database
worker. That runner does not exist today.

| Shape | Fits | When |
|---|---|---|
| **AMI** (EC2 with Postgres + pgbx extension) | self-managed Postgres on EC2; works with today's extension | could ship after 0.6 (PITR + encryption), low effort |
| **Container listing** (ECS/EKS/Cloud Run job) running a "pgbx runner" | RDS/Aurora/Cloud SQL customers, the bigger market | needs the runner mode (new ADR) plus adapters for IAM auth; after 0.6 |
| CLI only (no listing) | developers; already today via install.sh | now |

Recommendation: no listing before 0.6. Build the runner mode next if the managed-Postgres buyer is the target,
and list the container first (it covers the most buyers), with the AMI as an easy second.

## Open questions for the owner

1. **The SSH tunnel today is reused for 10 minutes** (ADR 0002, built on the owner's request). Should the
   `ssh` built-in keep that reuse, since it stores no secret, only host and port? Or should it re-run per command
   like every other adapter, to keep one rule? This draft keeps today's SSH reuse until decided.
2. Licence: MIT, or a one-time fee? This also decides the marketplace pricing model.
3. Marketplace target: managed Postgres (needs a new runner mode) or self-managed (an AMI with today's extension)?
4. Should custom adapter recipes be shared (a docs page of examples), or only the contract documented?
