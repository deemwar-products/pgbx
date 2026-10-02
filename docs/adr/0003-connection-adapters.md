# ADR 0003: Connection adapters and profiles (and a marketplace note)

Status: **Accepted in principle (owner decisions, 2026-10-02, including "no connection code in the core"); build
after the current merges.**
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

### Adapters: no connection code in pgbx

pgbx itself contains **no connection code**: no SSH, AWS, GCP or Azure logic. Every way of reaching a server is
an **external adapter**, a command listed in config. pgbx stays a single Rust binary with no Node (or any
other runtime) requirement. A runtime is needed only for the adapters a user enables.

```toml
[additional_adapters]
# defaults shipped in the repo under adapters/ (copied by the installer; enable by uncommenting)
# ssh   = ["node", "~/.pgbx/adapters/ssh/ssh-connector.js"]
# aws   = ["node", "~/.pgbx/adapters/aws/aws-connector.js"]
# gcp   = ["node", "~/.pgbx/adapters/gcp/gcp-connector.js"]
# azure = ["node", "~/.pgbx/adapters/azure/azure-connector.js"]
corp-vpn = ["/usr/local/bin/corp-pg", "--env", "prod"]   # any executable works the same way
```

**Contract (v1).** pgbx runs `<command> connect <name> [profile args...]` with no shell, in its own process
group (`setsid` on Unix, a Job Object on Windows), for **every command**. The adapter prints **one JSON line** on
stdout within `ready_timeout` (default 30 s), then **exits**:

```json
{"url": "postgres://user:pass@127.0.0.1:54321/shop?sslmode=require", "state": "ready", "name": "prod-eu"}
```

| Field | Meaning |
|---|---|
| `url` | full connection string, password included if needed. pgbx uses it **in memory for this run only** and saves it nowhere (not in profiles, state, memories or logs). Output redacts it to `postgres://user:***@...`. Child tools get the password through their environment, never their argv. |
| `state` | shown as is. `ready` means go; anything else (`error: no credentials for project acme-prod`, `mfa required`) stops the command with a non-zero exit. |
| `name` | the connection's name. It must match the profile name (a mismatch is an error). With an ad-hoc adapter it names the memory folder (`~/pgbx/<name>/<db>/`). |

- **The adapter owns reuse and caching.** It may start a **detached** tunnel process that it owns (an SSH forward,
  `aws ssm start-session`, `cloud-sql-proxy`), print the line and exit. On the next call it checks whether its
  tunnel is still up, reuses it, or restarts it if broken, with its own idle expiry (the default adapters use 10
  minutes). pgbx's built-in 10-minute tunnel reuse (`cli/src/tunnel.rs`) is **removed**.
- **pgbx stops only the adapter process**: SIGTERM to its process group, then SIGKILL after 5 s, if it hasn't
  exited after the ready-timeout or on Ctrl-C. It **never touches anything the adapter detached**.
- **An optional verb, `<command> stop <name>`**, tears down that adapter's tunnel for `name`. It's used by
  `pgbx profile disconnect NAME`. An adapter without it answers with a non-zero exit, and pgbx reports "this
  adapter has no stop".
- **pgbx does nothing with secrets.** Credentials belong to the adapter, the vendor CLI or the user.

### Default adapters (in the repo, not in the binary)

`adapters/ssh`, `adapters/aws`, `adapters/gcp` and `adapters/azure`. Each has its own README (prerequisites, profile
args, how reuse and expiry work) and its own tests. They wrap the user's existing tooling and credentials:

| Adapter | Wraps |
|---|---|
| `ssh` | system `ssh` (keys, agent, `~/.ssh/config`, ProxyJump); a detached `ssh -N -L` forward, reused, with a 10-minute idle expiry |
| `aws` | `aws ssm start-session` port forwarding to RDS or EC2, plus `aws rds generate-db-auth-token` for IAM auth when asked |
| `gcp` | `cloud-sql-proxy` (and `gcloud` for the instance name / IAM) |
| `azure` | `az` (Bastion tunnel and Entra ID token for Azure Database for PostgreSQL) |

Custom adapters follow exactly the same contract, and we embed none of their code.

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

- The core gets smaller: `cli/src/tunnel.rs` (its SSH spawn, helper, state files, lock and 10-minute reuse) moves
  out into `adapters/ssh`. The client is profiles, the adapter contract and process-group handling.
- One rule for every connection, built-in or custom. Profiles are the product's centre for the client side.
- Enabling a default adapter needs Node on that machine. pgbx itself still doesn't.
- **Host-side commands** (`doctor`, `logs`, `setup server`, `diagnose`) ran on the server over pgbx's own SSH
  (`ssh target pgbx <cmd>`). With no SSH code in the core they need another route. See Q1.
- Tests:
  - pgbx: a fake adapter that prints ready and exits; prints an error state; never prints; prints a mismatching
    name; ignores SIGTERM (so SIGKILL must land); detaches a child (which must survive pgbx); implements `stop`.
    The URL must never appear on disk or in output.
  - Each default adapter: its own tests (reuse, broken-tunnel restart, idle expiry, `stop`).

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

1. **Host-side commands without SSH in the core.** `doctor`, `logs`, `setup server` and `diagnose` need to run on the
   database host. Options: (a) an optional adapter verb `<command> exec <name> -- pgbx <cmd> --json`, which the ssh
   adapter implements and the cloud ones may (SSM `send-command`); (b) the user runs them on the host
   themselves. This draft proposes (a).
2. **Default adapters in Node**: fine for developer laptops, but servers and Windows boxes often lack Node.
   Ship them as Node only, or also as POSIX `sh` / PowerShell versions for `ssh`, the most common one?
3. Licence: MIT, or a one-time fee? This also decides the marketplace pricing model.
4. Marketplace target: managed Postgres (needs a new runner mode) or self-managed (an AMI with today's extension)?
5. Should custom adapter recipes be shared (a docs page of examples), or only the contract documented?
