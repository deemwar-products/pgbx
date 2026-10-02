---
title: Data scope
description: Which tables keep their rows in a per-database backup.
sidebar: { order: 4 }
---

Every table **definition** is always backed up, so a restore never misses a table.
Data scope only decides which tables keep their **rows**.

- Default: every table, every row.
- `exclude`: skip rows of these tables.
- `include`: keep rows of only these tables.
- Patterns: `schema.table`; no schema means `public`; `*` and `?` are wildcards.
- pgbx's own tables are never skipped.

```sql
SELECT pgbx.set_data_scope(exclude => ARRAY['public.sessions', 'audit_log_*']);
SELECT * FROM pgbx.rowless_tables();   -- tables whose rows the next backup skips
SELECT pgbx.set_data_scope();          -- back to everything
```

Data scope applies to every backup of this database, scheduled or manual.
How-to: [Data scope guide](../../guides/data-scope/).
