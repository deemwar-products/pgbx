---
title: Migrating from pgbackrestx
description: Move a server from the old pgbackrestx extension to pgbx 0.5.0.
sidebar: { order: 12 }
---

pgbx is the new name of pgbackrestx, starting fresh at 0.5.0. An extension cannot be renamed, so there is no
`ALTER EXTENSION ... UPDATE` from pgbackrestx: you install pgbx next to it, then drop the old one.
Your old backups stay restorable.

## 1. Install pgbx and switch the settings

1. Install the pgbx package (extension files + `pgbx` CLI).
2. In `postgresql.conf` (and `postgresql.auto.conf` if you used `ALTER SYSTEM`):
   - `shared_preload_libraries`: replace `pgbackrestx` with `pgbx`.
   - Rename every `pgbackrestx.*` setting to `pgbx.*` (`s3_endpoint`, `s3_bucket`, `s3_region`, `server_name`,
     `credentials_file`, `socket_dir`, `poll_seconds`, `max_days_limit`, `alert_command`, `dump_compression`,
     `audit_days`, `admin_db`). The credentials file can stay where it is: point `pgbx.credentials_file` at it.
   - Delete the removed ones: `cluster_backups`, `cluster_schedule`, `cluster_retention_full`, `cluster_full_every`,
     `cluster_process_max`, `restore_from_system_id`, `wal_queue_max`, `wal_alert_after`, `wal_alert_size`,
     `wal_gap_margin`, `work_dir`, `stop_command`, `start_command`.
3. Restart Postgres.

The worker then creates `pgbx` in every database and in `template1`, and resets the `archive_command` that
pgbackrestx set — only if it is exactly `pgbackrest --config=<dir>/pgbackrest.conf --stanza=main archive-push %p`
(logged; any other `archive_command` is left alone). `archive_mode` is left alone: pgbx does not need it, so turn
it off at the next planned restart if nothing else uses it.

## 2. Drop the old extension

In each database (and `template1`):

```sql
-- optional: keep the old audit trail first
COPY pgbackrestx.history TO '/tmp/<db>-pgbackrestx-history.csv' CSV HEADER;
DROP EXTENSION pgbackrestx CASCADE;
```

Re-grant your people `pgbx_viewer` / `pgbx_admin`, then `DROP ROLE pgbackrestx_viewer, pgbackrestx_admin;`.
The old local work directory (`/var/lib/postgresql/pgbackrestx`) can be deleted.

## Old backups

The S3 layout is unchanged (`<server>/<db>/<timestamp>.dump`), so dumps taken by pgbackrestx are listed and
restored like new ones:

```sh
pgbx backups    --from-s3 --db shop --s3-endpoint URL --s3-bucket B --server-name S --credentials-file F
pgbx db-restore --from-s3 --db shop --into shop_old --s3-endpoint URL --s3-bucket B --server-name S --credentials-file F
```

The restore skips the old extension's objects; your data comes back.

Old whole-server data under `<server>/<system id>/_cluster/` (and `<server>/_cluster/`) is never touched.
Delete it yourself when you no longer need it, e.g.
`aws s3 rm --recursive s3://<bucket>/<server>/<system id>/_cluster/ --endpoint-url <endpoint>`
or `mc rm --recursive --force <alias>/<bucket>/<server>/<system id>/_cluster/`.
