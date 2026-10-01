---
title: Data scope
description: Skip the rows of big or throwaway tables.
sidebar: { order: 3 }
---

Table definitions are always backed up. Only rows are skipped. Background: [Data scope](../../concepts/data-scope/).

```sql
-- skip rows of these
SELECT pgbx.set_data_scope(exclude => ARRAY['public.sessions', 'audit_log_*']);
-- keep rows of only these
SELECT pgbx.set_data_scope(include => ARRAY['billing.*', 'users']);
-- what the next backup skips
SELECT * FROM pgbx.rowless_tables();
-- back to everything
SELECT pgbx.set_data_scope();
```

```sh
pgbx scope --db myapp                                    # show
pgbx scope --db myapp --exclude public.sessions --yes    # narrowing is guarded
pgbx scope --db myapp --reset
```

After a restore, skipped tables exist but are empty.
