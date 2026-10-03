# Alerts — recipes

Native notifications (Slack, Telegram, webhook, email) and Prometheus metrics. URLs/tokens are secrets: they
live only in `pgbx.notify_secrets_file`; never print that file or put a URL in a setting.

**User-visible formatting (family default):** which channels, what triggers a message.

---

### ALR-R-1: Send failures to Slack / Telegram / email / a webhook

**When to use:** "alert me on slack", "notify on backup failure", "telegram alert", "email when backups fail".

**Call sequence:**
1. Ask the human to add the secret lines to `/etc/pgbx/notify.secrets` (chown postgres, chmod 600), e.g.
   `slack.ops.url = https://hooks.slack.com/...`, `telegram.oncall.token = ...` + `telegram.oncall.chat_id = ...`,
   `webhook.pager.url = ...`, `email.dba.smtp = smtps://user:pass@host:465` + `email.dba.from` + `email.dba.to`.
2. Settings (human approval): `ALTER SYSTEM SET pgbx.notify = 'slack:ops'; ALTER SYSTEM SET pgbx.notify_secrets_file = '/etc/pgbx/notify.secrets'; SELECT pg_reload_conf();`
3. Check the Postgres log for `pgbx: notify` errors after the next failure.

**Expected response:** one message per incident (same db + kind + error not repeated for 6 h) and one "OK again".

**Common errors:** `the slack channel needs a NAME ..., not a URL` (Postgres log) → the setting holds names only; `slack.ops.url is missing` → secrets file line.

**User-visible formatting:** "Failures of <dbs> now go to <channels>; recovery is announced once."

### ALR-R-2: Prometheus metrics

**When to use:** "prometheus", "grafana", "pgbx metrics", "scrape metrics".

**Command:**
```bash
pgbx metrics                                  # one scrape, text
pgbx ui --listen 127.0.0.1:8432               # GET /metrics (read-only; exposing beyond localhost = human's call)
```

**Expected response:** `pgbx_up 1`, `pgbx_last_backup_age_seconds{database="myapp"} 3600`, `pgbx_restore_test_ok`, `pgbx_failed_jobs`, `pgbx_queue_depth`.

**Common errors:** `pgbx_up 0` / HTTP 503 → cannot read the admin database (see STAT-R-5); columns missing → server older than 0.6.

**User-visible formatting:** "<db>: last backup <age> ago, restore test <ok/failed>, <n> failed jobs."
