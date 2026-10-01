# Step 01 — Backup before deploy

Tier: safe mutation. Five steps, in order; stop at the first failure.

1. **Health** — STAT-R-1. If `state` is `failing`, stop: "Backups are failing (`<last_error>`); fix that before deploying."
2. **Backup** — BKP-R-1 (`pgbx now --db {db} --wait --json`). Several databases: repeat per database.
3. **Wait** — `--wait` blocks until `done`/`failed`. SQL fallback: BKP-R-3 polling loop.
4. **Verify** — VFY-R-1 (`pgbx verify --wait --json`).
5. **Report** — one line: "Backup #<id> of {db} done (<size>, <s3_key>), restore test ok. Safe to deploy."
   On failure: quote `error`, say "do not deploy", wait for the human.
