---
title: Notifications and metrics
description: Slack, Telegram, webhook and email alerts; a Prometheus /metrics endpoint.
sidebar: { order: 15 }
---

## Channels

`pgbx.notify` lists channels **by name**. The URLs and tokens live only in a secrets file (settings are
readable by every role; the file is readable by postgres only).

```ini
pgbx.notify              = 'slack:ops, telegram:oncall, webhook:pager, email:dba'
pgbx.notify_secrets_file = '/etc/pgbx/notify.secrets'      # chown postgres, chmod 600
```

```ini
# /etc/pgbx/notify.secrets
slack.ops.url           = https://hooks.slack.com/services/T000/B000/XXXX
telegram.oncall.token   = 123456:ABC-your-bot-token
telegram.oncall.chat_id = -1001234567890
webhook.pager.url       = https://example.com/pgbx-hook
webhook.pager.header    = Authorization: Bearer your-token        # optional
email.dba.smtp          = smtps://user:password@smtp.example.com:465   # smtp://host:587 = STARTTLS
email.dba.from          = pgbx@example.com
email.dba.to            = dba@example.com, ops@example.com
```

A name can be left out (`slack`): it then means `slack.default.url`. A URL written into `pgbx.notify` itself is
refused (logged as "needs a NAME, not a URL"), and that message never repeats what was written. A secrets file that
group or others can read is refused.

## What is sent

- A **failed** backup, restore, restore test or point-in-time base backup: one message per incident. The same
  database + job kind + error is not repeated for 6 hours; a different error is sent at once.
- **OK again**: one message when that database's job of that kind next succeeds.
- With [point-in-time restore](../point-in-time-restore/): WAL archiving stuck or failing (`wal_archive`, a reminder
  every hour, one "recovered") and WAL dropped (`wal_gap`), for the whole server.
- Slack/Telegram/email get a short text; the webhook gets JSON:

```json
{"event":"failure","server":"db-prod-1","database":"shop","kind":"backup","job_id":42,
 "error":"...","at":"2026-10-01T02:00:03Z","text":"pgbx: backup of shop on db-prod-1 FAILED ..."}
```

Messages are sent by the job's thread (or a thread of their own for whole-server incidents), never by the worker's
poll loop, with a 10 s timeout per channel. Errors while sending go to the Postgres log (prefix `pgbx:`) and never
include a URL or token. `pgbx.alert_command` keeps working as before: it runs on **every** failure.

## Prometheus

`pgbx ui` serves `GET /metrics` (read-only, same login as the [audit UI](../audit-ui/)); `pgbx metrics` prints one
scrape.

```yaml
scrape_configs:
  - job_name: pgbx
    static_configs: [{ targets: ['db-prod-1:8432'] }]   # pgbx ui --listen 0.0.0.0:8432 behind your firewall
```

| metric (label `database`) | meaning |
|---|---|
| `pgbx_up` | 1 when the overview could be read (0, HTTP 503 otherwise) |
| `pgbx_databases` | databases pgbx looks after (no label) |
| `pgbx_last_backup_timestamp_seconds`, `pgbx_last_backup_age_seconds` | newest completed backup |
| `pgbx_last_backup_size_bytes`, `pgbx_backups_kept` | size and count |
| `pgbx_failed_jobs` | failed backups / restores / restore tests in the retained history (`pgbx.audit_days`) |
| `pgbx_restore_test_ok` | 1 passed / 0 failed (absent: never ran) |
| `pgbx_queue_depth` | jobs waiting |
| `pgbx_last_backup_encrypted` | 1 when the newest backup is [encrypted](../encryption/) |
| `pgbx_overview_age_seconds` | worker heartbeat for that row |
| `pgbx_database_state{state=...}` | active / failing / paused / ... |

```yaml
# alert: no backup for 26 hours
- alert: PgbxBackupStale
  expr: pgbx_last_backup_age_seconds > 26*3600
```

Exposing `pgbx ui` beyond `127.0.0.1` is your call; it only reads.
