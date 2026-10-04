# pgbx 0.6.1: full test suite, 2026-10-04

Where: deemwar-dev-agent (x86_64, 4 cores, rootless Docker, **shared**: load average 5-19 from other work during the
runs), PostgreSQL 16, a throwaway local S3 (RustFS) with generated keys. The pgbx code under test is v0.6.1; the
commits after the tag change only tests, docs, CI, `Cargo.lock` and package metadata.

## Result

| suite | passed | failed | log |
|---|---|---|---|
| CLI unit tests (`cargo test --locked --manifest-path cli/Cargo.toml`) | 147 (143 + 4) | 0 | full-suite-82ee437.log |
| e2e | 85 | 0 | full-suite-82ee437.log |
| cli_e2e | 68 | 0 | full-suite-82ee437.log |
| ui_e2e | 23 | 0 | full-suite-82ee437.log |
| serve_e2e | 73 | 0 | full-suite-82ee437.log |
| queue_e2e | 62 | **1** | full-suite-82ee437.log, rerun-bench-queue-9f4fadb.log |
| load_e2e | 32 | 0 | full-suite-82ee437.log |
| extras_e2e | 59 | 0 | full-suite-82ee437.log |
| pitr_e2e | 49 | 0 | full-suite-82ee437.log (no 0.5.0 image on this box: the update-script section is skipped) |
| s3down_e2e | 11 | 0 | full-suite-82ee437.log |
| imds_e2e | 39 | 0 | full-suite-82ee437.log |
| https_s3_e2e (real AWS S3 over https, dummy keys) | 6 | 0 | full-suite-82ee437.log |
| bench_local | 11 | 0 | rerun-bench-queue-9f4fadb.log |
| upgrade_e2e | 6 | 0 | full-suite-82ee437.log |
| **end-to-end total** | **524** | **1** | |

The one failure is real and filed: on a busy server the time estimate applies a x3 "server busy" factor even to a
backup limited by `pgbx.upload_kbps`, so it says ~95 s for a 31 s job
([issue #2](https://github.com/deemwar-products/pgbx/issues/2)). It passes on an idle machine.

## What the earlier runs found (fixed in the tests, not in pgbx)

`full-suite-b18630e-first-run.log` is the first run on this box (kept as found):
- imds_e2e 18/21: `minio/minio` no longer exists on Docker Hub (quay.io needs a login, dl.min.io answers 410).
  Now Chainguard's MinIO images (43f821b).
- e2e 1 failure: the pg_restore nice/IO check caught the worker's short `pg_restore --version` probe on the slow box
  (the same race was already fixed for pg_dump). Fixed (82ee437).
- load_e2e 2 failures: the host's own load average (other work on the box) kept the load gate busy. The test now
  drives "busy" only from its own pgbench (82ee437).
- bench_local in the second run: its S3 outage started at a fixed +4 s, before the queued restore had even listed its
  dumps; it now starts mid-transfer (9f4fadb). That start-up list has no retry:
  [issue #1](https://github.com/deemwar-products/pgbx/issues/1).
