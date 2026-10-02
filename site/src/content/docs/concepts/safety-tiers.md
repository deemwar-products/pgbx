---
title: Safety tiers
description: How pgbx and the agent skill classify every action.
sidebar: { order: 6 }
---

Every `pgbx` command has a safety level. It is enforced in code and printed as `safety` in `--json` output: `readonly`, `safe`, `guarded` or `destructive`.

| level | commands | rule |
|---|---|---|
| read-only | `status`, `list`, `backups --from-s3`, `doctor`, `diagnose`, `logs`, `overview`, policy commands with no arguments | never mutate anything |
| safe | `now`, `verify`, `db-restore`, `resume`, `link`, `schedule TEXT`, `skill`, `db-restore --from-s3` | queue jobs or write only somewhere new; `db-restore` refuses an existing database or the source |
| guarded | `pause`, lowering `retention`, narrowing `scope`, `verify-schedule never` | refused without `--yes`, with an explanation of what would be lost |

No pgbx command overwrites a live database: every restore goes into a NEW database. Swapping names
(`ALTER DATABASE ... RENAME`) or dropping the old one is left to a human.

For AI agents these flags are the human's signature. See [Safety for agents](../../agents/skill/).
