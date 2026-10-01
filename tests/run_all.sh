#!/usr/bin/env bash
# Full verification from scratch: rebuild the image, fresh containers, unique S3 folder per run, then
#   1) tests/e2e.sh        — every per-database feature against the real S3 bucket
#   2) tests/cli_e2e.sh    — the pgbx CLI (policy, refusals, skill, db-restore --from-s3 onto a server without pgbx)
#      tests/ui_e2e.sh     — pgbx ui: read-only audit UI (pages, JSON, GET only, viewer role)
#   3) tests/bench_local.sh — ~1.2 GB speed + S3-outage chaos against a local S3
#   4) tests/upgrade_e2e.sh — worker auto-update (ALTER EXTENSION UPDATE incl. template1), leftover archive_command
#      reset, and (PGBX_OLD_IMAGE set) restoring the old product's dumps
# PG_MAJOR=13..18 picks the Postgres major (needs a dev image built with that PG_MAJOR, see docker/Dockerfile).
set -u
cd "$(dirname "$0")/../docker"
export PGBX_SERVER="pgbx-test-$(date -u +%Y%m%d%H%M%S)"
echo "== run $PGBX_SERVER"
docker compose -f compose.test.yml down -v >/dev/null 2>&1
docker compose -f compose.local.yml down -v >/dev/null 2>&1
docker compose -f compose.test.yml build -q || exit 1
docker compose -f compose.test.yml up -d >/dev/null 2>&1
# wait for the REAL server: the image first runs a temporary one for initdb and stops it, so a fixed sleep is not enough
for _ in $(seq 300); do docker compose -f compose.test.yml logs db 2>&1 | grep -q "init process complete" && break; sleep 1; done
ok=0; for _ in $(seq 120); do
  if docker compose -f compose.test.yml exec -T db psql -U postgres -qAt -c "SELECT 1" >/dev/null 2>&1; then ok=$((ok+1)); [ $ok -ge 3 ] && break; else ok=0; fi; sleep 1
done
echo "== server ready"
../tests/e2e.sh; e2e=$?
../tests/cli_e2e.sh; cli=$?
../tests/ui_e2e.sh; ui=$?
../tests/bench_local.sh; bench=$?
(set -a; . ./.env; set +a; ../tests/upgrade_e2e.sh); upgrade=$?
diag=0
if [ "${PGBX_DIAG_SMOKE:-0}" = 1 ]; then   # optional: pgbx doctor/diagnose with pg_wal piling up, no S3 (throwaway container)
  (cd .. && docker run --rm --name pgbx-diag-smoke -v "$PWD":/src -v pgbx-diag-target:/src/target \
     -v pgbx-diag-clitarget:/src/cli/target -w /src pgbx-dev sh tests/diag_smoke.sh); diag=$?
fi
echo "== e2e exit $e2e, cli_e2e exit $cli, ui_e2e exit $ui, bench exit $bench, upgrade exit $upgrade, diag smoke exit $diag"
[ $e2e -eq 0 ] && [ $cli -eq 0 ] && [ $ui -eq 0 ] && [ $bench -eq 0 ] && [ $upgrade -eq 0 ] && [ $diag -eq 0 ]
