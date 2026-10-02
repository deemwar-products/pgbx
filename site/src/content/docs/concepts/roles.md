---
title: Roles
description: Who can call what.
sidebar: { order: 5 }
---

The extension creates two roles, server-wide. `PUBLIC` gets nothing.

| role | can |
|---|---|
| nobody (`PUBLIC`) | nothing |
| `pgbx_viewer` | `status()`, `backups`, `history`, `config`, `overview()`, `doctor()`, `rowless_tables()`, `pitr_status()`, `pitr_state` |
| `pgbx_admin` | viewer + `configure()`, `set_schedule()`, `set_retention()`, `pause()`, `resume()`, `backup_now()`, `restore()`, `verify_now()`, `set_verify_schedule()`, `download_url()`, `set_data_scope()` |

Admin functions run as `SECURITY DEFINER`, so admins never need write access to the tables. `pitr_backup_now()`
is for superusers only. Your application's own roles are kept with every backup: see
[Roles with every backup](../../guides/roles/).

```sql
GRANT pgbx_admin  TO my_migrator;   -- migrations can call configure()
GRANT pgbx_viewer TO grafana;
```

`overview()` and `doctor()` run only in the admin database (`pgbx.admin_db`, default `postgres`).
