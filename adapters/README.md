# pgbx connection adapters (examples)

An adapter is any program that hands pgbx a Postgres connection string. pgbx contains no connection code. These
four are **examples** you can enable, copy, or replace with your own, in any language
([ADR 0003](../docs/adr/0003-connection-adapters.md)).

| Adapter | Reaches Postgres through | Needs |
|---|---|---|
| [ssh](ssh/) | the system `ssh` (keys, agent, `~/.ssh/config`, ProxyJump) | `ssh` |
| [aws](aws/) | AWS Systems Manager port forwarding to RDS or EC2, optional RDS IAM auth | `aws` CLI + session-manager-plugin |
| [gcp](gcp/) | Cloud SQL Auth Proxy, optional IAM auth | `cloud-sql-proxy`, gcloud credentials |
| [azure](azure/) | direct to a flexible server, or an Azure Bastion tunnel; optional Entra ID auth | `az` CLI |

They are Node scripts (Node 18+, no dependencies). Node is needed only on a machine that enables one, and pgbx
itself never needs it.

## Enable one

```yaml
# pgbx config
adapters:
  ssh: node /path/to/pgbx/adapters/ssh/ssh-adapter.js
profiles:
  prod:
    adapter: ssh
    target: ops@db1.example.com
    user: app
    password: $PGPASSWORD        # expanded by pgbx from the environment / .env / your secret handler
    dbname: shop
```

## The protocol (v1)

1. pgbx starts the command and writes one line on stdin: `{"action":"start","name":"prod","config":{...}}`. The
   `config` is the profile's settings with `$VAR`s expanded.
2. The adapter connects and prints **exactly one line** on stdout:
   `{"url":"postgres://...","state":"ready","name":"prod"}`. On failure, `state` starts with `error:`.
   Logs go to stderr only.
3. The adapter keeps its tunnel up until pgbx writes `{"action":"stop"}`, or until stdin closes. Then it cleans up and
   exits.

`lib/adapter.js` implements this for the examples: `run(async (name, config) => ({ url, cleanup }))`.

## Tests

```sh
cd adapters && npm test      # protocol tests with fake vendor tools; no cloud accounts needed
bash test/ssh_real.sh          # the ssh adapter against a real sshd + Postgres in Docker (bash 4+)
```
