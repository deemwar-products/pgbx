---
title: Upgrading
description: Move to a new pgbx version.
sidebar: { order: 11 }
---

1. Install the new extension files and restart Postgres (the worker is a preloaded library).
2. The worker runs `ALTER EXTENSION pgbx UPDATE` by itself in every database and in `template1` whenever the
   installed version is older than the library's, and logs it. New databases are therefore never born at an old
   version. To do it by hand:

```sql
ALTER EXTENSION pgbx UPDATE;
```

3. Update the `pgbx` CLI by re-running the installer: `curl -fsSL https://deemwar-products.github.io/pgbx/install.sh | sh` (it also refreshes the agent skill).

pgbx starts fresh at 0.5.0. Coming from the previous product name? See
[Migrating from the previous version](../migrating/).
