---
title: Pause & resume
description: Stop automatic backups for a while.
sidebar: { order: 2 }
---

```sql
SELECT pgbx.pause('migrating, back at 6pm');
SELECT pgbx.resume();
```

- Pause stops **automatic** backups. `backup_now()` still works.
- The reason and time show in `status()` (`paused_reason`, `paused_at`).
- Pause and resume are recorded in `pgbx.history`.

```sh
pgbx pause --db myapp --reason 'migrating, back at 6pm' --yes   # guarded: needs --yes
pgbx resume --db myapp                                           # safe
```

To turn backups off for good (recorded): `SELECT pgbx.configure(enabled => false);`.
