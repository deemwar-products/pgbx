# Access — recipes

Roles and download links. Never grant roles to yourself; never paste a presigned link into chat.

**User-visible formatting (family default):** what changed, who can now do what.

---

### ACC-R-1: Who can do what

**When to use:** "who can run backups", "grant backup access", permission denied.

**Command:**
```bash
psql -XAtq -d myapp -c "GRANT pgbx_admin TO my_migrator"
```
| role | can |
|---|---|
| PUBLIC | nothing |
| `pgbx_viewer` | `status()`, `backups`, `history`, `overview()`, `doctor()` (admin db only), `pgbx ui` |
| `pgbx_admin` | + schedule, retention, pause/resume, backup_now, restore, verify, download_url, configure, data scope |
| superuser | + server-wide settings |

Granting is the human's call (ask first).

**Expected response:** `GRANT ROLE`.

**Common errors:** must be run by a role with admin option / superuser.

**User-visible formatting:** "<role> can now <capabilities>."

### ACC-R-2: Download link for a backup

**When to use:** "download the backup", "give me the dump file".

**Command:**
```bash
URL=$(pgbx link --db myapp --expires '15 minutes')        # --json gives {"url":..,"expires":..}; --backup-id N
curl -s "$URL" | pg_restore -d scratch_db
```
Fallback (SQL): `URL=$(psql -XAtq -d myapp -c "SELECT pgbx.download_url(expires => '15 minutes')")`
Specific backup: `pgbx.download_url(backup_id => 42, expires => '1 hour')`. Expiry 1 minute .. 7 days.
Long expiry or sharing with a third party = ask first. Every call is logged in `history`.

**Expected response:** a presigned HTTPS URL (keep it in `$URL`; never print it).

**Common errors:** "no such backup in this database" → STAT-R-2; expiry out of range; permission denied → `pgbx_admin`.

**User-visible formatting:** "Downloaded backup #<id> into scratch_db" — not the URL.
