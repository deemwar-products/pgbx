# Changelog

## 0.5.0

- **Renamed to pgbx** (was pgbackrestx): extension, schema, `pgbx.*` settings, roles `pgbx_viewer`/`pgbx_admin`,
  worker "pgbx scheduler", skill `pgbx-skill`. pgbx starts fresh at 0.5.0; see
  `site/src/content/docs/guides/migrating.md` (Migrating from pgbackrestx). Old dumps stay restorable.
- **Per-database backups only.** Removed: whole-server pgBackRest backups, WAL archiving, whole-server
  point-in-time restore, the second background worker, `cluster_status()`, `cluster_backups()`,
  `cluster_backup_now()`, `cluster_restore()`, `cluster_info`, the WAL doctor rows, settings `cluster_backups`,
  `cluster_schedule`, `cluster_retention_full`, `cluster_full_every`, `cluster_process_max`,
  `restore_from_system_id`, `wal_queue_max`, `wal_alert_after`, `wal_alert_size`, `wal_gap_margin`, `work_dir`,
  `stop_command`, `start_command`; CLI `pgbx restore`, `now --cluster`, `--system-id`, `logs --pgbackrest`;
  the UI's Whole-server screen. No pgBackRest package or `archive_mode` needed.
- New: `pgbx backups --from-s3` and `pgbx db-restore --from-s3 [--backup KEY | --time TS]` — restore onto a new
  server without the extension (its objects are skipped), streamed with resumable download.
- PostgreSQL 13–18. `pgbx.dump_compression` defaults to `auto` (zstd:3 with pg_dump 16+, gzip level 6 before);
  the newest installed pg_dump/pg_restore is used.
- The worker runs `ALTER EXTENSION pgbx UPDATE` itself when a database or `template1` is older than the library,
  and resets the `archive_command` pgbackrestx set (only that one).
- doctor(): extension loaded, s3 settings, credentials file, database backups, restore tests, workers,
  archive_mode (info), replication_slots.
- Fix: the prune job no longer stays `running` (invalid JSON in its result).
- Docker images take a `PG_MAJOR` build arg.
