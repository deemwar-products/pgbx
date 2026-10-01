---
title: Postgres down?
description: Use pgbx diagnose to find out why.
sidebar: { order: 10 }
---

When Postgres will not start, SQL is gone. `pgbx` still works.

```sh
pgbx diagnose
pgbx diagnose --log /var/log/postgresql/postgresql-16-main.log --pgdata /var/lib/postgresql/16/main --json
docker logs db 2>&1 > /tmp/pg.log && pgbx diagnose --log /tmp/pg.log
```

It reads (each optional): `postmaster.pid` and liveness, `pg_ctl status`, the newest server log,
`journalctl -u postgresql*`, `--log FILE`, the kernel log (OOM kills), disk usage of the data dir, `pg_wal`,
replication slots holding WAL, `pgsql_tmp`, the log dir, and the data dir owner/mode.

Probable causes: `oom_kill`, `disk_full`, `wal_backlog`, `stale_pid`, `crash`, `corruption`, `permissions`,
`config_error`, `too_many_connections`, `unknown`.

```json
{"ok": true, "command": "diagnose", "safety": "readonly",
 "postgres": "down", "probable_cause": "disk_full",
 "evidence": [{"source": "...", "line": "..."}],
 "steps": [{"tier": "destructive", "why": "...", "command": "...", "needs_human_approval": true}],
 "facts": {}}
```

- pgbx **never runs a step**. Destructive steps are marked for human approval.
- `pg_wal`, `base`, `global` and `pg_xact` are never offered for deletion.
- `pgbx doctor` runs the diagnosis automatically when Postgres is unreachable (`diagnosis` in its JSON).
- Lost the server for good? `pgbx db-restore --from-s3` restores each database onto a new one; see [Disaster recovery](../disaster-recovery/).
