# Backup — recipes

Tier: safe mutation. Role: `pgbx_admin` (per database).

**User-visible formatting (family default):** "Backup #<id> of <db> done: <size>, <s3_key>."

---

### BKP-R-1: Back up one database now

**When to use:** "take a backup", "backup now", "back up myapp".

**Command:**
```bash
pgbx now --db myapp --wait --json
```
Fallback (SQL): `psql -XAtq -d myapp -c "SELECT pgbx.backup_now()"` → returns the history id; then BKP-R-3.

**Expected response:** `{id, state: "done", s3_key, bytes}`; key shape `s3://<bucket>/<server_name>/<db>/<UTC timestamp>.dump`.

**Common errors:**
- `failed` with S3 error → STAT-R-5; transient network errors retry once.
- permission denied → needs `pgbx_admin`.

**User-visible formatting:** family default.

### BKP-R-3: Wait for a queued job (SQL fallback)

**When to use:** after a SQL `backup_now()` / `restore()` / `verify_now()`.

**Command:**
```bash
JOB=$(psql -XAtq -d myapp -c "SELECT pgbx.backup_now()")
until S=$(psql -XAtq -d myapp -c "SELECT state FROM pgbx.history WHERE id = $JOB"); [ "$S" = done ] || [ "$S" = failed ] || [ "$S" = expired ]; do sleep 5; done
psql -XAtq -d myapp -c "SELECT row_to_json(h) FROM (SELECT id, state, s3_key, bytes, error FROM pgbx.history WHERE id = $JOB) h"
```

**Expected response:** final row with `state` and `s3_key` or `error`.

**Common errors:** loops forever on `queued` → worker not running; stop after ~10 min and report.

**User-visible formatting:** family default, or the error verbatim.
