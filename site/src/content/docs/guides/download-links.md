---
title: Download links
description: A presigned URL for one backup file.
sidebar: { order: 8 }
---

```sql
SELECT pgbx.download_url();                                   -- newest backup, 1 hour
SELECT pgbx.download_url(backup_id => 42, expires => '15 minutes');
```

```sh
url=$(pgbx link --db myapp --expires '1 hour')
curl -s "$url" | pg_restore -d scratch
```

- `expires` must be between 1 minute and 7 days.
- Every link issued is logged in `history` with who asked.
- The link is a bearer token. Do not paste it into chat or tickets. `pgbx link` prints only the URL.
- Only `pgbx_admin` can call it; `PUBLIC` has no access.
