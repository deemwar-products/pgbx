# Policy — recipes

Schedule changes and resume are safe. Pausing > 1 h, lowering retention, narrowing data scope: ask first.
Role: `pgbx_admin`. Run inside the database. Confirm every change with STAT-R-1.

**User-visible formatting (family default):** the function's returned sentence, plus the new `status()` value.

---

### POL-R-1: Set the schedule

**When to use:** "backup every hour", "change backup schedule".

**Command:**
```bash
pgbx schedule --db myapp --json                    # show current + next run
pgbx schedule 'every 1 hour' --db myapp --json
```
Fallback (SQL): `psql -XAtq -d myapp -c "SELECT pgbx.set_schedule('every 1 hour')"`
Forms: `'every 15 minutes'`, `'daily at 02:30'`, `'weekly on sunday at 03:00'`, `'hourly'`, `'daily'`, `'weekly'`, cron `'0 */6 * * *'`.

**Expected response:** `schedule set to "every 1 hour" (cron …); next backup at …`.

**Common errors:** bad schedule text → rejected immediately.

**User-visible formatting:** family default.

### POL-R-2: Retention

**When to use:** "keep backups for 30 days", "retention".

**Command:**
```bash
pgbx retention --db myapp --json                   # show
pgbx retention --db myapp --max-backups 14 --max-days 30 --json   # LOWERING is refused without --yes
```
Fallback (SQL): `psql -XAtq -d myapp -c "SELECT count(*) FROM pgbx.backups WHERE age > interval '30 days'"` then
`psql -XAtq -d myapp -c "SELECT pgbx.set_retention(max_backups => 14, max_days => 30)"`
Whichever deletes first wins; the newest backup is always kept; prunes now. LOWERING = ask first, quoting the count from the first line.

**Expected response:** `keeping at most 14 backups and nothing older than 30 days (newest always kept); pruning now`.

**Common errors:** `max_days … exceeds this server's limit` → `pgbx.max_days_limit` (server setting).

**User-visible formatting:** family default + "<n> older backups will be deleted".

### POL-R-3: One-call policy (migrations)

**When to use:** setting policy from a migration.

**Command:**
```bash
psql -XAtq -d myapp -c "SELECT row_to_json(c) FROM pgbx.configure(schedule => 'every 6 hours', max_backups => 28, max_days => 30) c"
```
NULL arguments leave settings unchanged. `enabled => false` disables backups = destructive tier.

**Expected response:** the full `pgbx.config` row.

**Common errors:** same as POL-R-1 / POL-R-2.

**User-visible formatting:** schedule + retention from the row.

### POL-R-4: Pause and resume

**When to use:** "pause backups during the migration", "resume backups".

**Command:**
```bash
pgbx pause --db myapp --reason 'migrating, back at 6pm' --yes --json   # --yes only after the user agreed
pgbx resume --db myapp --json
```
Fallback (SQL): `psql -XAtq -d myapp -c "SELECT pgbx.pause('migrating, back at 6pm')"` / `... -c "SELECT pgbx.resume()"`
Always pass a reason. Pause > 1 h = ask first. Manual backups keep working while paused.

**Expected response:** `automatic backups paused (…); resume with SELECT pgbx.resume()` / `automatic backups resumed; next backup at …`.

**Common errors:** none typical.

**User-visible formatting:** family default + how to resume.

### POL-R-5: Data scope

**When to use:** "exclude sessions table rows", "backup only billing data".

**Command:**
```bash
pgbx scope --db myapp --json                                  # show data_scope + rowless_tables
pgbx scope --db myapp --exclude 'public.sessions,audit_log_*' --json   # narrowing: refused without --yes
pgbx scope --db myapp --include 'billing.*,users' --json
pgbx scope --db myapp --reset --json                          # everything again (safe)
```
Fallback (SQL): `psql -XAtq -d myapp -c "SELECT pgbx.set_data_scope(exclude => ARRAY['public.sessions', 'audit_log_*'])"`,
`set_data_scope(include => ARRAY[...])`, `set_data_scope()`, `SELECT json_agg(table_name) FROM pgbx.rowless_tables()`
Every table DEFINITION is always backed up; include/exclude decide whose ROWS are kept. Narrowing = ask
first, listing `rowless_tables()`. Resetting with `set_data_scope()` is safe.

**Expected response:** `backups keep every table definition; rows skipped for N table(s) right now …`.

**Common errors:** bare names mean `public.<name>`; `*` and `?` are wildcards.

**User-visible formatting:** "Rows skipped for: <tables>."

### POL-R-6: Quietest time to back up (suggest, never auto-apply)

**When to use:** "when should backups run", "backups slow the app down", "move backups to a quiet time",
"suggest a schedule", "pgbx schedule suggest", doctor's `schedule_in_quiet_window` warning.

**Command:**
```bash
pgbx schedule suggest --db myapp --json                    # read-only: the suggestion + apply_sql
pgbx schedule suggest --db myapp --apply --yes --json      # only after the human agreed to the new time
```
Fallback (SQL): `psql -XAtq -d myapp -c "SELECT row_to_json(w) FROM pgbx.suggest_window() w"`

**Expected response:** `suggestion` = `start_at` ('daily 03:00 UTC'), `cron`, `score` vs `current_score` (activity
relative to an average hour), `confidence` (`none` | `low` | `high`), `est_duration`, `days_sampled`; `apply_sql` =
`SELECT pgbx.configure(schedule => '...')` to copy into a migration; `applied`.

**Common errors:** `confidence: none` → no activity learned yet (hours of sampling needed); do not apply. `low` → say
so and prefer waiting a week. `refusing without --yes` → the schedule is never changed without the human.

**User-visible formatting:** "Quietest: <start_at> (<score>x vs <current_score>x now, <confidence> confidence); apply
with: <apply_sql>". Never apply on your own.
