---
title: Disaster recovery on a new server
description: Restore databases from S3 onto a fresh server, with or without the extension installed.
sidebar: { order: 6 }
---

Every per-database backup is a plain `pg_dump` custom-format file at
`s3://<bucket>/<server_name>/<database>/<UTC timestamp>.dump`. When the old server is gone, `pgbx` reads
those dumps straight from S3 and restores them into the new server. The target does **not** need
pgbx; it needs `pg_restore` on `PATH` and a role that can create databases.

## 1. A credentials file

```
access_key_id=...
secret_access_key=...
```

Same format as the extension's `pgbx.credentials_file`. `chmod 600` it. pgbx never prints the keys.

## 2. List what is there

```sh
pgbx backups --from-s3 --db shop \
  --s3-endpoint https://hel1.your-objectstorage.com --s3-bucket my-backups --s3-region hel1 \
  --server-name db1 --credentials-file ./s3.credentials
```

Lists the dumps under `s3://my-backups/db1/shop/`, newest first (`key`, `taken_at`, `bytes`).
`--db` is the folder name (the database's `path`, which defaults to its name).

## 3. Restore into a new database

```sh
pgbx db-restore --from-s3 --db shop --into shop_restored \
  --time '2026-09-30 18:00:00+00' \
  --s3-endpoint https://hel1.your-objectstorage.com --s3-bucket my-backups --s3-region hel1 \
  --server-name db1 --credentials-file ./s3.credentials \
  --host /var/run/postgresql --user postgres
```

- Picks the newest dump taken at or before `--time` (default: the newest). `--backup <key>` picks one exactly.
  `--time` must carry a UTC offset.
- Creates `--into` first and **refuses if it already exists**.
- Streams the dump into `pg_restore --no-owner`. A dropped connection resumes from the byte it reached
  (HTTP Range), so a running restore never starts over.
- If the target server has no pgbx library, the pgbx schema/extension is left out, so only your
  data comes back. With the extension installed, the database keeps its backup policy and history.

Repeat per database. Once verified, rename: `ALTER DATABASE shop_restored RENAME TO shop;`.
