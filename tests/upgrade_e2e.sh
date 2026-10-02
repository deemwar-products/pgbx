#!/usr/bin/env bash
# Upgrade harness (bash 3.2-safe). Throwaway container pgbx-upgrade and volume pgbx-upgrade-data
# (both removed at the end). Uses the same S3 env as compose.test.yml (S3_ENDPOINT, S3_BUCKET, S3_REGION,
# docker/test.credentials) and a unique server folder.
#
# 1. auto-update: a database (and template1) on an OLDER pgbx version is updated by the worker itself
#    (ALTER EXTENSION pgbx UPDATE), so new databases are never born at an old version. The older version is faked
#    with a copy of the install script (pgbx--0.0.1.sql) + an empty update script (pgbx--0.0.1--<current>.sql).
set -u
cd "$(dirname "$0")/../docker"
NEW=${PGBX_NEW_IMAGE:-pgbx:test}
C=pgbx-upgrade; VOL=pgbx-upgrade-data; SERVER="pgbx-upgrade-$(date -u +%Y%m%d%H%M%S)"
: "${S3_ENDPOINT:?set S3_ENDPOINT (as for compose.test.yml)}" "${S3_BUCKET:?}" "${S3_REGION:?}"
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "  PASS $1"; pass=$((pass+1)); else echo "  FAIL $1 (got '$2', want '$3')"; fail=$((fail+1)); fi; }
P() { docker exec -u postgres "$C" psql -v ON_ERROR_STOP=1 -qAt "$@"; }
wait_up() { local ok=0; for _ in $(seq 240); do
  if P -c "SELECT 1" >/dev/null 2>&1; then ok=$((ok+1)); [ $ok -ge 3 ] && return 0; else ok=0; fi; sleep 1; done; return 1; }
start() { # image extension-name
  docker rm -f "$C" >/dev/null 2>&1
  docker run -d --name "$C" -e POSTGRES_PASSWORD=test-only-not-secret -v "$VOL":/var/lib/postgresql/data \
    -v "$PWD/test.credentials":/etc/$2/s3.credentials:ro "$1" postgres \
    -c shared_preload_libraries=$2 -c $2.s3_endpoint="$S3_ENDPOINT" -c $2.s3_bucket="$S3_BUCKET" \
    -c $2.s3_region="$S3_REGION" -c $2.server_name="$SERVER" \
    -c $2.credentials_file=/etc/$2/s3.credentials -c $2.poll_seconds=3 >/dev/null || return 1
  for _ in $(seq 300); do docker logs "$C" 2>&1 | grep -q "init process complete\|PostgreSQL Database directory appears to contain a database" && break; sleep 1; done
  wait_up; }
ver() { P -d "$1" -c "SELECT extversion FROM pg_extension WHERE extname='pgbx'" 2>/dev/null; }
cleanup() { docker rm -f "$C" >/dev/null 2>&1; docker volume rm "$VOL" >/dev/null 2>&1; }
trap cleanup EXIT
docker image inspect "$NEW" >/dev/null 2>&1 || { echo "missing $NEW (docker compose -f compose.test.yml build)"; exit 1; }
cleanup

echo "## 1. current image ($NEW)"
start "$NEW" pgbx || { echo "server not up"; exit 1; }
for _ in $(seq 60); do [ -n "$(ver postgres)" ] && break; sleep 2; done
want=$(P -c "SELECT default_version FROM pg_available_extensions WHERE name='pgbx'")
check "admin db at $want" "$(ver postgres)" "$want"

echo "## 2. auto-update: a database on an older version is updated by the worker"
EXT=$(P -c "SELECT setting FROM pg_config WHERE name='SHAREDIR'")/extension
docker exec -u root "$C" sh -c "cp $EXT/pgbx--$want.sql $EXT/pgbx--0.0.1.sql && echo '-- test only' > $EXT/pgbx--0.0.1--$want.sql"
P -c "CREATE DATABASE upd" >/dev/null
for _ in $(seq 60); do [ -n "$(ver upd)" ] && break; sleep 1; done
P -d upd -c "BEGIN; DROP EXTENSION pgbx; CREATE EXTENSION pgbx VERSION '0.0.1'; COMMIT;" >/dev/null
for _ in $(seq 30); do [ "$(ver upd)" = "$want" ] && break; sleep 1; done
check "worker updated upd 0.0.1 -> $want" "$(ver upd)" "$want"
check "update logged" "$(docker logs "$C" 2>&1 | grep -c "upd: updated extension 0.0.1 -> $want")" 1
P -d template1 -c "BEGIN; DROP EXTENSION pgbx; CREATE EXTENSION pgbx VERSION '0.0.1'; COMMIT;" >/dev/null
check "template1 at the fake old version" "$(ver template1)" 0.0.1

echo "## 3. at worker start template1 is updated and new databases are born current"
docker restart -t 30 "$C" >/dev/null; wait_up
for _ in $(seq 30); do [ "$(ver template1)" = "$want" ] && break; sleep 1; done
check "template1 updated at worker start" "$(ver template1)" "$want"
P -c "CREATE DATABASE born" >/dev/null
check "a new database is born at $want" "$(ver born)" "$want"

echo "== upgrade_e2e: $pass passed, $fail failed (S3 folder $SERVER)"
[ $fail -eq 0 ]
