---
title: Connect through SSH, AWS, GCP, Azure or your own adapter
description: Reach Postgres behind a bastion, SSM, the Cloud SQL Auth Proxy, Azure Bastion or your own tooling with an adapter, any program that hands pgbx a connection string.
sidebar: { order: 0 }
---

pgbx has no SSH or cloud code of its own. When Postgres is not reachable directly, a profile names an
**adapter**: a program that sets up the way in (a tunnel, a proxy, a token) and prints one connection string.
pgbx starts it when a command needs Postgres and stops it when the command ends. `pgbx serve` keeps one running
for its whole run.

Four examples ship with pgbx, and you can write your own in any language. The installer copies the examples to
`~/.config/pgbx/adapters/` (`%APPDATA%\pgbx\adapters` on Windows). They are Node scripts, so they need Node 18+
on your machine; pgbx itself does not.

Passwords are always `$VAR` references in single quotes; pgbx reads them from your environment (or your
[secrets source](../../reference/config/#secrets)) when a command runs and never stores or prints them.

## SSH

Uses your own `ssh`: keys, agent, `~/.ssh/config` and ProxyJump. It runs
`ssh -N -o ExitOnForwardFailure=yes -o BatchMode=yes -o ServerAliveInterval=30 -L 127.0.0.1:<free>:<pg_host>:<pg_port> target`.
`BatchMode` means no password prompt: use a key or your agent.

```sh
pgbx profile add prod --adapter ssh target=ops@db.example.com user=app 'password=$PGPASSWORD' dbname=shop
pgbx profile add behind --adapter ssh target=ops@db.internal jump=bastion.example.com user=postgres
pgbx profile add other-box --adapter ssh target=ops@bastion pg_host=10.0.3.7 pg_port=6432 user=app
pgbx status --profile prod
```

| setting | default | |
|---|---|---|
| `target` | (required) | `user@host` or a `~/.ssh/config` Host |
| `ssh_port` | 22 | |
| `jump` | | ProxyJump host(s), `-J` |
| `pg_host`, `pg_port` | `localhost`, `5432` | Postgres as seen **from the SSH host** |
| `user`, `password`, `dbname`, `sslmode` | | go into the connection string |
| `ready_timeout_ms` | 20000 | how long ssh may take |

## AWS (Systems Manager)

Reaches RDS, Aurora or Postgres on EC2 through SSM port forwarding: no open port, no bastion, your own `aws`
credentials. Needs the AWS CLI v2 and the session-manager-plugin.

```sh
pgbx profile add prod-eu --adapter aws target=i-0abc123def4567890 \
  db_host=shop.cluster-xyz.eu-west-1.rds.amazonaws.com region=eu-west-1 aws_profile=acme-prod \
  user=app 'password=$SHOP_DB_PASSWORD' dbname=shop ready_timeout=60s
```

| setting | default | |
|---|---|---|
| `target` | (required) | an SSM-managed instance id that can reach the database |
| `db_host`, `db_port` | `localhost`, `5432` | the RDS endpoint as that instance sees it |
| `region`, `aws_profile` | | `--region`, `--profile` |
| `user`, `password`, `dbname`, `sslmode` | | |
| `iam_auth` | | `true`: an RDS IAM token as the password (and `sslmode=require`) |

## GCP (Cloud SQL)

Runs the Cloud SQL Auth Proxy (v2) with your gcloud / Application Default Credentials. The proxy encrypts the
hop to Google, so the local connection is plain.

```sh
pgbx profile add analytics --adapter gcp instance=acme-prod:europe-west1:analytics user=app 'password=$PGPASSWORD' dbname=events
```

| setting | | |
|---|---|---|
| `instance` | (required) | `project:region:instance` |
| `iam_auth` | | `true`: `--auto-iam-authn` (the `user` is your IAM principal) |
| `private_ip` | | `true`: `--private-ip` |
| `user`, `password`, `dbname` | | |

## Azure

Uses your `az` login, either straight to a flexible server (`host`) or through `az network bastion tunnel` to
Postgres on a VM.

```sh
pgbx profile add vm-db --adapter azure bastion=corp-bastion resource_group=rg-data \
  target_resource_id=/subscriptions/.../virtualMachines/db1 user=postgres 'password=$PGPASSWORD'
```

| setting | | |
|---|---|---|
| `host` | direct | e.g. `shop.postgres.database.azure.com` |
| `bastion`, `resource_group`, `target_resource_id` | bastion | |
| `db_port` | 5432 | |
| `user`, `password`, `dbname`, `sslmode` | | `sslmode` defaults to `require` |
| `entra_auth` | | `true`: an Entra ID token as the password |

### TLS

pgbx speaks TLS to Postgres with libpq's `sslmode` (see [TLS in the config reference](../../reference/config/#tls)),
so servers that force it work: Azure flexible server, RDS with `rds.force_ssl` (the default from PostgreSQL 15),
and every URL that says `sslmode=require` (`iam_auth`, `entra_auth`, the azure adapter's default). An adapter's
`sslmode` setting goes into its URL, and that wins over everything else.

To check the server's certificate, not only encrypt: through a tunnel (aws, azure bastion, ssh) pgbx connects to
`127.0.0.1`, which is not the name in the certificate, so use `sslmode=verify-ca` with the provider's CA bundle in
`sslrootcert` (for example AWS's `global-bundle.pem`); straight to a server (azure `host`) `verify-full` works,
the public roots and your OS store are built in. The gcp adapter's local hop stays `sslmode=disable`: the proxy
encrypts the way to Google.

## Write your own

An adapter is any program. It gets one JSON line on stdin and prints one JSON line on stdout:

```text
stdin  <- {"action":"start","name":"prod","config":{"env":"prod","password":"..."}}
stdout -> {"url":"postgres://app:...@127.0.0.1:54321/shop","state":"ready","name":"prod"}
stdin  <- {"action":"stop"}        (or stdin closes: pgbx is gone)
```

The rules:

- `config` is the profile's settings (everything but pgbx's own keys), with `$VAR`s already expanded.
- Print **exactly one line** on stdout, within the profile's `ready_timeout` (30 s by default). Logs go to
  stderr: pgbx shows stderr (masked) when something fails, and treats anything else on stdout as a protocol error.
- On failure, print a `state` other than `ready` (for example `"error: no route to db"`) and exit non-zero.
- `name` must be the profile name pgbx sent.
- Keep your tunnel or proxy running until `{"action":"stop"}` **or** stdin closes, then clean up and exit.
  pgbx waits 5 s, then kills your whole process group, so children you started go too.
- Caching, retries and keeping the tunnel healthy are your adapter's job; pgbx never restarts it.

A complete adapter in Python that asks a company tool for a short-lived URL:

```python
#!/usr/bin/env python3
import json, subprocess, sys

start = json.loads(sys.stdin.readline())
name, cfg = start["name"], start["config"]
try:
    url = subprocess.run(["corp-db", "url", "--env", cfg["env"]], check=True,
                         capture_output=True, text=True).stdout.strip()
except Exception as e:
    print(f"corp-db failed: {e}", file=sys.stderr)
    print(json.dumps({"url": "", "state": f"error: {e}", "name": name}), flush=True)
    sys.exit(1)
print(f"got a url for {cfg['env']}", file=sys.stderr)
print(json.dumps({"url": url, "state": "ready", "name": name}), flush=True)
for line in sys.stdin:            # wait for stop, or EOF when pgbx exits
    if '"stop"' in line:
        break
```

Enable it and try it:

```sh
pgbx profile add corp --adapter corp --adapter-command 'python3 /opt/corp/pgbx-adapter.py' env=prod
pgbx query "SELECT current_user" --profile corp
```

The reply to `profile add` says exactly what the adapter command runs. Test an adapter by hand without pgbx:

```sh
printf '{"action":"start","name":"corp","config":{"env":"prod"}}\n' | python3 /opt/corp/pgbx-adapter.py
```

It should print one JSON line and exit when the pipe closes. The examples in the repo's `adapters/` folder
share a small helper (`adapters/lib/adapter.js`) you can copy.

## Troubleshooting

| message | meaning |
|---|---|
| `gave no result within 30 s` | the adapter never printed its line: raise `ready_timeout`, or check its stderr (shown after the message) |
| `protocol error: the adapter printed a line that is not its JSON result` | something went to stdout before the result: send logs to stderr |
| `adapter state: error: ...` | the adapter said why it failed; its stderr follows |
| `answered for 'x', not for profile 'y'` | the adapter must echo the `name` it was given |
| `exited without a result` | the adapter crashed or exited before printing (missing tool, bad settings) |
| `$NAME is not set` | a `$VAR` in the profile is not in your environment or secrets source |
| `no adapter 'NAME'` | add it with `--adapter-command`, or copy the examples to `~/.config/pgbx/adapters` |
| `` `pgbx diagnose` is host-side: run `pgbx diagnose` on the database host `` | host-side commands (`diagnose`, `setup server`) read the server's own files: run them there |

See [Configuration](../../reference/config/) for the whole file, the secrets sources and the protocol.
