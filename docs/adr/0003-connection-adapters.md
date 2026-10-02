# ADR 0003: Connection adapters and profiles (and a marketplace note)

Status: **Accepted, final shape (owner decisions, 2026-10-02). Built for 0.6.0 (branch `adr0003-impl`):**
one `config.yaml` (YAML; the TOML below was the sketch) with `adapters`, `profiles` and `secrets`;
`pgbx profile add | edit | remove | list | show | use` and `--url` / `PGBX_URL`; `$VAR` expansion from the
environment, a .env file or a handler command; adapter protocol v1 (process group / Job Object, start and stop,
one result line, timeouts, Ctrl-C); the ssh / aws / gcp / azure examples in `adapters/`; pgbx's built-in SSH
(`tunnel.rs`, `pgbx tunnel`, `--ssh*`) removed and `profiles.json` migrated automatically. **Not built:** the
`exec` action (Q1: host-side commands run on the host), the marketplace runner, and TLS for adapter URLs (the CLI
still connects with NoTls, so servers that force TLS cannot be reached yet). Reference:
`site/src/content/docs/reference/config.md`.
Builds on ADR 0002 (profiles, SSH tunnels, read queries).

## Context

Today a profile reaches Postgres directly (`--host/--port`) or over SSH (`cli/src/tunnel.rs`: a detached helper
owns `ssh -N -L ...`; it's reused across commands and closes after 10 idle minutes). Real databases often sit
behind cloud tooling: AWS (SSM Session Manager port forwarding, RDS IAM auth), GCP (`cloud-sql-proxy`) and Azure
(`az` / Bastion). Or they sit behind a company's own script. Users also have **many** of them: several AWS
accounts, GCP projects and environments.

## Decision

pgbx contains **no connection code** and **stores no secrets**. A connection is either a plain connection
string or an **adapter**: an external command, in any language, that hands pgbx a connection string.

### Config: adapters and profiles are separate

(Sketch in TOML; as built, the file is `config.yaml` with the same `adapters:` and `profiles:` maps plus
`default:` and `secrets:`.)

```toml
[adapters]                        # defined once: name -> command (any executable; the defaults are examples)
ssh = "node ~/.pgbx/adapters/ssh/ssh-adapter.js"
aws = "node ~/.pgbx/adapters/aws/aws-adapter.js"
corp = "/usr/local/bin/corp-pg"

[profiles.prod-eu]                # an adapter + whatever config THAT adapter wants (free-form)
adapter  = "aws"
account  = "acme-prod"
region   = "eu-west-1"
instance = "shop-db"
url_user = "$PGUSER"

[profiles.dev]                    # or just a plain connection string
url = "postgres://$PGUSER:$PGPASSWORD@localhost:5432/shop"
```

- **Profiles are the core:** `pgbx profile add | edit | remove | list | show | use`, for many profiles (several
  AWS accounts, GCP projects, environments). Without a profile: `--url ...` / `PGBX_URL` for that run.
- **Credentials belong to the user.** A connection string naturally carries the user and password. pgbx expands
  `$VAR` / `${VAR}` references **anywhere in a profile or connection string** from the environment **at run time**
  (`$$` is a literal `$`). A missing variable is an error that names the variable, never a value. pgbx writes
  nothing secret to disk: profiles hold the references, not the values. Where values come from is configurable (see Secrets below).
- Expanded values and URLs are used in memory only. They never appear in logs or output (`postgres://user:***@...`),
  and child tools get the password through their environment, never their argv.

### Secrets: where `$VAR` values come from

One setting in pgbx's config says where secrets come from:

```yaml
secrets: env                         # DEFAULT: the process environment only
# secrets: .secrets/.env             # a .env file (relative to the config file, or absolute)
# secrets: node secret-handler.js    # a command (any executable, no shell)
```

Resolution order for each `$VAR`:
1. the **process environment**, always first, so `PGPASSWORD=... pgbx ...` always wins;
2. then the configured source, if any: a **.env file** (`KEY=value` lines, `#` comments, optional quotes), or a
   **secret handler command**.

For a command, pgbx runs it once per variable, with the **variable name** as its only argument
(`node secret-handler.js PGPASSWORD`). The command prints the value on stdout (a trailing newline is stripped)
and exits 0. A non-zero exit or empty output is an error that names the variable, never a value. The value is
used in memory only, never stored or logged, and redacted in output.

pgbx ships **no secret-manager integrations**. Users write their handler for AWS Secrets Manager, Vault, `sec`,
1Password and so on, the same way as adapters (any language; a few lines). Timeouts: 10 s per variable by default.

### Adapter protocol (v1)

1. pgbx starts the adapter's command (no shell; its own process group: `setsid` on Unix, a Job Object on
   Windows), with stdin and stdout as pipes.
2. pgbx writes **one line on stdin**: `{"action":"start","name":"prod-eu","config":{...}}`. The `config` is that
   profile's settings with `$VAR`s expanded; the adapter decides what it needs.
3. The adapter connects (caching, retries and keeping its tunnel or proxy up are **its own job**) and prints
   **exactly one line on stdout**, its result:
   ```json
   {"url": "postgres://ops:secret@127.0.0.1:54321/shop?sslmode=require", "state": "ready", "name": "prod-eu"}
   ```
   `state` other than `ready` means failure, and pgbx shows it. `name` must match the profile name.
   **stdout carries only this result.** All of the adapter's logs go to its own log (stderr or a file).
4. pgbx reads that one line, connects with the URL, and **does not talk to or restart the adapter after that**.
5. When pgbx is done (a one-off command finishes; `pgbx serve` shuts down), it writes `{"action":"stop"}` on
   stdin. **stdin closing also means stop**, so if pgbx dies the adapter exits too. The adapter cleans up and exits.

**Timeouts:** no result line within `ready_timeout` (default 30 s) means failure, and pgbx shows the adapter's
stderr (redacted). After `stop`, a grace period (default 5 s), then pgbx kills the adapter's process group
(SIGTERM, then SIGKILL; on Windows it terminates the Job Object).

**Lifetime:** a one-off CLI command starts its adapter and stops it when done; `pgbx serve` keeps one adapter alive
per connection for its whole run. No detached tunnels, idle expiry or state files in pgbx. pgbx's built-in
SSH tunnel and reuse (`cli/src/tunnel.rs`) are removed and become the `ssh` example adapter.

### Default adapters: examples, in the repo, not in the binary

`adapters/ssh`, `adapters/aws`, `adapters/gcp` and `adapters/azure`, each with its own README (prerequisites, the
profile config keys it reads) and tests. They're **example commands** users can enable, copy or replace, in any
language. Node is never a pgbx dependency.

| Adapter | Wraps |
|---|---|
| `ssh` | system `ssh` (keys, agent, `~/.ssh/config`, ProxyJump), running `ssh -N -L 127.0.0.1:<free>:<pg>` as its child until stop |
| `aws` | `aws ssm start-session` port forwarding to RDS or EC2, plus `aws rds generate-db-auth-token` for IAM auth when asked |
| `gcp` | `cloud-sql-proxy` (and `gcloud` for the instance name / IAM) |
| `azure` | `az` (Bastion tunnel and Entra ID token for Azure Database for PostgreSQL) |

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

- The core gets smaller: profiles, `$VAR` expansion (env, .env or a handler command), a two-message stdin protocol, one stdout line, and
  process-group handling. `cli/src/tunnel.rs` moves out into `adapters/ssh`.
- One rule for every connection, and nothing outlives pgbx.
- Each one-off command pays its adapter's start-up (seconds for SSM or cloud-sql-proxy). `pgbx serve` pays it once.
  Adapters may cache internally.
- **Host-side commands** (`doctor`, `logs`, `setup server`, `diagnose`) ran over pgbx's own SSH. With no SSH in the core,
  in v1 they run on the host itself (the user runs `pgbx doctor` there). See Q1.
- Tests:
  - pgbx: a fake adapter that answers ready; answers an error state; never answers (timeout, stderr shown);
    prints logs on stdout before the result (protocol error, clearly reported); ignores `stop` (the group is
    killed after the grace period); and pgbx killed mid-run (stdin closes, the adapter exits). `$VAR` expansion,
    a missing variable, `$$`; .env parsing; a handler command that succeeds, fails, prints nothing or hangs. No secret on disk or in output.
  - Each default adapter: its own tests.

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

1. **Host-side commands** (`doctor`, `logs`, `setup server`, `diagnose`) over an adapter: an optional `exec` action
   is undecided and **out of v1**.
2. Licence: MIT, or a one-time fee? This also decides the marketplace pricing model.
3. Marketplace target: managed Postgres (needs a new runner mode) or self-managed (an AMI with today's extension)?
4. Should custom adapter recipes be shared (a docs page of examples), or only the contract documented?
