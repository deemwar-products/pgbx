# ADR 0003: Connection adapters (and a marketplace note)

Status: **Draft, for discussion with the owner. Do not build yet.** Updated 2026-10-02 with the owner's
decisions: pgbx stores no secrets (connection string used in memory only); the pitch is "own your own Postgres
backups".
Date: 2026-10-02. Builds on ADR 0002 (profiles, SSH tunnels, read queries).

## Context

Today a profile reaches Postgres in two ways: directly (`--host/--port`), or over SSH
(`cli/src/tunnel.rs`). The SSH path spawns a detached helper, `pgbx tunnel --serve KEY`. The helper owns
`ssh -N -L 127.0.0.1:<free>:<pg host>:<pg port> target` and records `{pid, ssh_pid, port, last_used, idle_secs}`
in `<state dir>/tunnels/KEY.json` (0600). Later commands reuse it, and it exits after `tunnel-idle` (default
10 min). Host-side commands (`setup`, `doctor`, `logs`, `diagnose`) run as `ssh target pgbx <cmd> --json`.

Real databases often sit behind something other than plain SSH:

| Access path | Tool |
|---|---|
| AWS SSM Session Manager port forwarding | `aws ssm start-session` |
| GCP Cloud SQL | `cloud-sql-proxy` |
| Kubernetes | `kubectl port-forward` |
| Azure Bastion | `az network bastion tunnel` |
| Company-specific | a company's own script |

Every one of these follows the same shape: run a local helper process, wait until it says a local port is
ready, connect to it, and stop the helper afterwards.

## Decision (proposed)

A profile can name an **adapter**. SSH becomes the built-in default adapter, and anyone can plug in their own.

```toml
[profiles.prod]
adapter = "exec"                       # or a built-in: ssh (default when ssh= is set), aws-ssm, gcp-sql, kubectl, azure-bastion
command = ["node", "gcp-proxy.js"]     # argv, no shell; any executable
env     = { PROJECT = "acme-prod" }    # non-secret values only; secrets come from the environment / `sec`
user    = "postgres"
db      = "shop"
ready_timeout = "30s"
```

### The contract (protocol v1)

1. pgbx spawns `command` with no shell, in its **own process group**: `setsid` on Unix, a Job Object on
   Windows. stdin is closed, and stderr is captured (redacted, shown only on failure).
2. The adapter prints **one JSON line** on stdout within `ready_timeout`:
   - `{"v":1,"ready":true,"url":"postgres://user:pass@127.0.0.1:54321/shop?sslmode=require"}`, the full
     connection string, password included if needed; or
   - `{"v":1,"ready":true,"host":"127.0.0.1","port":54321}`, the host/port form, with credentials from the
     usual places (`~/.pgpass`, `PGPASSWORD`); or
   - `{"v":1,"ready":false,"error":"..."}`.
3. **pgbx does nothing with secrets** (owner decision, 2026-10-02). It uses the connection string in memory,
   for that run only, and **saves nothing**: not in profiles, not in tunnel state files, not in memories,
   not in logs. Any output that echoes a connection redacts the password (`postgres://user:***@...`). Child
   tools (`pg_dump`, `pg_restore`) get the password through their environment, never their argv, so it can't
   show up in `ps`. Profiles store only the adapter command and non-secret settings. The adapter, or the
   user, owns the credentials, and pgbx only forwards and uses them.
4. Lifecycle reuses today's tunnel helper. The helper owns the adapter instead of `ssh`, and the state file
   records only `adapter`, `pid`, `pgid`, `host` and `port`, never the URL. Reuse across commands works as
   today for the host/port form. When the adapter returned a URL with a password, that URL lives only in the
   process that received it, so a later command can't reuse it from disk. Either the adapter is run again for
   each command and stopped after it, or the helper hands it over a local, owner-only socket (open question Q2).
5. Stopping: on idle expiry, `pgbx tunnel close`, a crash or a ready-timeout, the helper sends **SIGTERM to the
   group**, then **SIGKILL** after 5 s. On Windows it terminates the Job Object. No orphan proxies.
6. Built-in adapters are thin wrappers around the vendor CLIs the user already has: `aws`, `cloud-sql-proxy`,
   `kubectl` and `az`. Their credentials stay with those CLIs, and pgbx only picks a free port and waits for
   it to accept connections. `ssh` stays exactly as it is, as one more built-in.
7. Adapters only carry **Postgres connections**. Host-side commands stay SSH-only. An adapter with no host
   access gets an honest "doctor/logs need the host; use an ssh profile for that". A later protocol v2
   could add `"exec":true` for adapters that can also run commands on the host (SSM `send-command`,
   `kubectl exec`).

### Safety

- A profile with an `exec` adapter runs arbitrary code, so profiles are as sensitive as `~/.ssh/config`:
  the profiles file stays 0600, `pgbx profile add --adapter exec` prints what it will run, and
  **`pgbx memories import` never carries profiles or adapters**.
- The agent skill may *use* a profile with an adapter, but never creates or edits one without the user
  asking. Adapters fall in the "safe" tier for using and the "guarded" tier for adding.
- No change to the read-only guard: `pgbx query` stays SELECT-only whatever the adapter.

### Prior art compared

| Mechanism | What we take | What we avoid |
|---|---|---|
| git credential helpers | exec a named helper, line protocol on stdout, stays out of the core | key=value text: we use one JSON line |
| kubectl exec credential plugins | versioned JSON contract (`apiVersion`), env passed explicitly, interactive mode is opt-in | none |
| Terraform `external` data source | program in, one JSON object out, non-zero exit = error | no lifecycle there; ours must keep a process alive and stop it |
| ssh `ProxyCommand` | the user's own command decides how to get there | stdio-only transport: we want a port so pg_dump/psql/pgbx share it |

## Consequences

- One code path for SSH and every cloud. The tunnel helper becomes generic, with SSH as a special case of it.
- Cloud users (RDS behind SSM, Cloud SQL, k8s) get profiles, `pgbx query`, memory and `db-restore --from-s3`
  without opening ports. This also widens the client-only use without changing pgbx's core.
- New surface: process-group handling on Windows, ready-timeout tuning, and stderr redaction.
- Testing: a fake adapter script in e2e (prints ready, then sleeps; prints an error; never prints; prints a URL
  with a password, which must work and then be found nowhere on disk or in output; ignores SIGTERM, so SIGKILL must
  land).

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

1. Is SSH as the "default adapter" the right framing, or should adapters stay a separate, advanced feature?
2. A URL with a password can't be stored for reuse. Do we start the adapter again for each command (simplest,
   slower), or let the helper hand the URL to later commands over a local owner-only socket (fast, more code)?
3. Which built-ins first? Suggestion: `kubectl` and `aws-ssm` (most asked for), then `gcp-sql` and `azure-bastion`.
4. Should adapters be shareable (a small registry or a docs page of recipes), or just documented as a contract?
5. Marketplace: is the target managed Postgres (needs the runner) or self-managed (the AMI works sooner)?
6. Marketplace pricing: free with paid support, hourly on the container, or BYOL? This affects whether
   anything beyond MIT is needed.
