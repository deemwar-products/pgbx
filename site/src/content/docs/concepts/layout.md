---
title: Folder layout
description: Where backups land in the bucket.
sidebar: { order: 2 }
---

```
s3://<bucket>/<server_name>/
  <database>/<UTC timestamp>.dump   per-database logical backups (pg_dump -Fc)
```

- `server_name` comes from `pgbx.server_name` (defaults to the hostname).
- The database folder is the `path` column of `pgbx.config` (defaults to the database name;
  change it with `configure(path => ...)`).
- Timestamp names sort chronologically; `pgbx backups --from-s3` and `restore()` rely on that.
- Installs older than 0.5.0 may also have a `<system_identifier>/_cluster/` folder from whole-server backups.
  It is left untouched; see [Disaster recovery](../../guides/disaster-recovery/#old-whole-server-data) to delete it.
