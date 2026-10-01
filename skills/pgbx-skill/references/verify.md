# Verify — recipes

`verify_now()` restores the newest backup into a scratch database, checks it, drops it: safe mutation.
`set_verify_schedule('never')` is higher-risk: ask first.

**User-visible formatting (family default):** "Restore test of <db>: ok (<checked>)" or "FAILED: <error>".

---

### VFY-R-1: Run a restore test now

**When to use:** "verify the backup", "can we actually restore".

**Command:**
```bash
pgbx verify --wait --json
```
Fallback (SQL): `psql -XAtq -d myapp -c "SELECT pgbx.verify_now()"` → history id; wait with BKP-R-3.

**Expected response:** `{id, state: "done"|"failed", error}`; `status().last_verify_result` becomes `ok: …` or `FAILED: …`.

**Common errors:** tests the NEWEST backup — wait for a just-queued backup to be `done` first.

**User-visible formatting:** family default.

### VFY-R-2: Verify schedule

**When to use:** "test restores daily", "change restore test schedule".

**Command:**
```bash
pgbx verify-schedule 'weekly on sunday at 04:00' --db myapp --json   # 'never' is refused without --yes
```
Fallback (SQL): `psql -XAtq -d myapp -c "SELECT pgbx.set_verify_schedule('weekly on sunday at 04:00')"`
Also `'daily at 05:00'`; `'never'` disables (higher-risk: ask first).

**Expected response:** sentence with the next run time; confirm via STAT-R-1 `verify_schedule`.

**Common errors:** invalid schedule text is rejected immediately.

**User-visible formatting:** "Restore tests: <schedule>, next at <time>."

### VFY-R-3: A verify failed — what it means

**When to use:** `last_verify_result` starts with `FAILED:`.

**Call sequence:**
1. Treat the database as **not backed up**, even if `state` is `active`.
2. Read the error: S3/download (transient → rerun VFY-R-1), `pg_restore` error (missing extension/role), disk full.
3. Take BKP-R-1, then VFY-R-1 again.
4. Still failing → report both errors. Do not lower retention (older good backups may be the only restorable ones), do not pause.
5. Prove an older backup with RST-R-1 at an earlier `--time`.

**Expected response:** a passing VFY-R-1 or an escalation to the human.

**Common errors:** none beyond the above.

**User-visible formatting:** "Newest backup of <db> does NOT restore: <error>. Next: <step>."
