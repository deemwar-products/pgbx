# ssh adapter

Forwards a free local port to Postgres on (or behind) an SSH host with `ssh -N -L`, using your normal SSH setup:
keys, agent, `~/.ssh/config` and ProxyJump. The adapter never reads a key.

| config key | required | default | |
|---|---|---|---|
| `target` | yes | | `user@host` or a `~/.ssh/config` alias |
| `ssh_port` | | 22 | |
| `jump` | | | ProxyJump host(s), `-J` |
| `pg_host` / `pg_port` | | `localhost` / `5432` | Postgres as seen from the SSH host |
| `user`, `password`, `dbname`, `sslmode` | | | go into the connection string (use `$VAR`s for secrets) |
| `ready_timeout_ms` | | 20000 | |

Runs `ssh -N -o ExitOnForwardFailure=yes -o BatchMode=yes -o ServerAliveInterval=30 -L 127.0.0.1:<free>:<pg_host>:<pg_port> [-p] [-J] target`.
BatchMode means no password prompts: use keys or an agent. `PGBX_SSH_BIN` overrides the ssh binary (tests).
