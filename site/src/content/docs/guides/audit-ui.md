---
title: Audit UI
description: pgbx ui, a read-only web view of every database's backups and history.
sidebar: { order: 9 }
---

```sh
pgbx ui --user pgbx_ui --strict          # then open http://127.0.0.1:8432/
```

Screens: **Overview** (every database: state, schedule, last/next backup, last restore test, last error),
**Timeline** (every `history` row of the last 30 days across all databases, filterable) and **Health**
(`doctor()` rows; fixes are shown, never run).

- Only `GET` is served; every connection runs `SET default_transaction_read_only = on`.
- A role that could change backups gets a warning; `--strict` refuses to start. Use a viewer login:
  `CREATE ROLE pgbx_ui LOGIN PASSWORD '...' IN ROLE pgbx_viewer;`
- Binds to 127.0.0.1 by default. Prefer an SSH tunnel over exposing it.
