---
title: Restore tests (verify)
description: Prove backups restore, on a schedule.
sidebar: { order: 7 }
---

A backup that has never been restored is not a backup. Verify restores the newest backup into a scratch
database, runs checks, writes the result to `history` and drops the scratch database. Failures alert.

```sql
SELECT pgbx.verify_now();                                   -- job id
SELECT pgbx.set_verify_schedule('weekly on sunday at 04:00'); -- the default
SELECT pgbx.set_verify_schedule('never');                   -- disable
SELECT last_verified_at, last_verify_result FROM pgbx.status();
```

```sh
pgbx verify --db myapp --wait
pgbx verify-schedule 'daily at 05:00' --db myapp
pgbx verify-schedule never --db myapp --yes     # guarded
```
