# Step 02 — Restore one database

Tier: safe mutation (into a NEW database). Swapping it in for the live one is destructive.

## Decision tree
1. Postgres down? → step-04 (diagnose). Server lost for good? → step-03.
2. Damage limited to one database (bad migration, deleted rows, dropped table)? → this step.
3. Several databases? → repeat this step per database. Precision is the backup schedule (no PITR).
4. Just need the dump file elsewhere? → ACC-R-2 download link.

## Flow
1. Pick the time: ask "restore to when?" if not given; default newest. Per-database granularity is the
   backup schedule (newest dump at or before the time) — list candidates with STAT-R-2.
2. Pick a new name: `{db}_restored_<yyyymmddhhmm>`; confirm it does not exist.
3. Run RST-R-1 and wait for `done`.
4. Report: "Restored {db} as of <time> into <new db>. Live {db} untouched." Offer to compare tables.
5. Renaming/dropping the live database → destructive: ask, quoting what is replaced.
