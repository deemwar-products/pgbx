# Step 04 — Postgres down / disk full / pg_wal growing

Tier: **read-only to diagnose.** pgbx never deletes, restarts or changes anything; every remedy is a printed
step with a tier, and you decide (or ask) per step.

1. **Diagnose (read-only):** DIAG-R-1 `pgbx diagnose --json` (add `--log FILE` when the log lives elsewhere, e.g.
   `docker logs db > /tmp/pg.log`). If Postgres is up, also DIAG-R-2 (doctor slot rows).
2. **Report:** one line `postgres: <state>, probable cause: <cause>`, the 1–3 evidence lines that prove it, and
   the space table when disk is involved.
3. **Walk `steps` in order, by tier** (workflow.md):
   - `readonly` → run it, show the output.
   - `safe` → run it after saying what it does (deleting OLD server logs, starting Postgres, restore a copy into a NEW database).
   - `guarded` → ask first, quoting `why` (e.g. chown, raising `max_wal_size` limits).
   - `destructive` (`needs_human_approval: true`) → ask for explicit approval of THAT step, quoting the
     consequence (e.g. dropping a replication slot: its consumer must be rebuilt). Never batch approvals.
4. **Never:** delete anything in `pg_wal/`, `base/`, `global/`, `pg_xact/`; run `pg_resetwal`; repair
   corruption in place. For `corruption`, follow DIAG-R-4 (restore a copy into a new database first).
5. **After:** re-run DIAG-R-1 / `pgbx doctor --json`. Only if the server is lost for good go to
   step-03-disaster-restore.md (restore each database from S3 onto a new server).
