# Changelog

## 0.5.0

First public release.

- Per-database backups: every database (and `template1`) gets the extension; `pg_dump`/`pg_restore` streamed to
  and from S3. No `archive_mode` or extra package needed.
- `pgbx backups --from-s3` and `pgbx db-restore --from-s3 [--backup KEY | --time TS]` — restore onto a new
  server without the extension (its objects are skipped), streamed with resumable download.
- PostgreSQL 13–18. `pgbx.dump_compression` defaults to `auto` (zstd:3 with pg_dump 16+, gzip level 6 before);
  the newest installed pg_dump/pg_restore is used.
- The worker runs `ALTER EXTENSION pgbx UPDATE` itself when a database or `template1` is older than the library.
- doctor(): extension loaded, s3 settings, credentials file, database backups, restore tests, workers,
  archive_mode (info), replication_slots.
- Docker images take a `PG_MAJOR` build arg.
