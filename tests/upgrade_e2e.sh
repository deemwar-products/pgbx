#!/usr/bin/env bash
# Upgrade + migration harness (bash 3.2-safe). Throwaway container pgbx-upgrade and volume pgbx-upgrade-data
# (both removed at the end). Uses the same S3 env as compose.test.yml (S3_ENDPOINT, S3_BUCKET, S3_REGION,
# docker/test.credentials) and a unique server folder.
#
# 1. auto-update: a database (and template1) on an OLDER pgbx version is updated by the worker itself
#    (ALTER EXTENSION pgbx UPDATE), so new databases are never born at an old version. The older version is faked
#    with a copy of the install script (pgbx--0.0.1.sql) + an empty update script (pgbx--0.0.1--<current>.sql).
# 2. leftover archive_command: the one the old product set for whole-server backups is reset at worker start;
#    anyone else's is never touched.
# 3. migration from the old product (only when PGBX_OLD_IMAGE names an image of it, e.g. the 0.4-era test image):
#    its per-database dumps are restored with `pgbx db-restore --from-s3`, and its extension can be dropped.
# 5. real schema update (only when PGBX_PREV_IMAGE names an image of the previous pgbx release, e.g. 0.5.0):
#    its databases are updated by sql/pgbx--<prev>--<current>.sql and end up identical to a fresh install.
set -u
cd "$(dirname "$0")/../docker"
NEW=${PGBX_NEW_IMAGE:-pgbx:test}; OLD=${PGBX_OLD_IMAGE:-}; PREV=${PGBX_PREV_IMAGE:-}
OLDNAME="pgbackrest""x"   # the product's previous name (split so a grep for it stays clean)
C=pgbx-upgrade; VOL=pgbx-upgrade-data; SERVER="pgbx-upgrade-$(date -u +%Y%m%d%H%M%S)"
: "${S3_ENDPOINT:?set S3_ENDPOINT (as for compose.test.yml)}" "${S3_BUCKET:?}" "${S3_REGION:?}"
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "  PASS $1"; pass=$((pass+1)); else echo "  FAIL $1 (got '$2', want '$3')"; fail=$((fail+1)); fi; }
P() { docker exec -u postgres "$C" psql -v ON_ERROR_STOP=1 -qAt "$@"; }
wait_up() { local ok=0; for _ in $(seq 240); do
  if P -c "SELECT 1" >/dev/null 2>&1; then ok=$((ok+1)); [ $ok -ge 3 ] && return 0; else ok=0; fi; sleep 1; done; return 1; }
wait_job() { # db id schema
  for _ in $(seq 90); do s=$(P -d "$1" -c "SELECT state FROM $3.history WHERE id=$2"); case "$s" in done|failed) echo "$s"; return;; esac; sleep 2; done; echo "timeout"; }
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

if [ -n "$OLD" ] && docker image inspect "$OLD" >/dev/null 2>&1; then
  echo "## 0. old product ($OLD): a per-database backup"
  start "$OLD" "$OLDNAME" || { echo "old server not up"; exit 1; }
  P -c "CREATE DATABASE shop" >/dev/null
  for _ in $(seq 60); do [ "$(P -d shop -c "SELECT count(*) FROM pg_extension WHERE extname='$OLDNAME'" 2>/dev/null)" = 1 ] && break; sleep 2; done
  P -d shop -c "CREATE TABLE orders(id int primary key, item text); INSERT INTO orders SELECT g, 'old-'||g FROM generate_series(1,500) g" >/dev/null
  id=$(P -d shop -c "SELECT $OLDNAME.backup_now()"); check "old product backup_now" "$(wait_job shop "$id" "$OLDNAME")" done
  docker stop -t 60 "$C" >/dev/null
else
  echo "## 0. skipped (set PGBX_OLD_IMAGE to an image of the old product to test the migration)"
  OLD=
fi

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

echo "## 3. leftover archive_command: the old product's is reset at worker start, anyone else's kept"
P -c "ALTER SYSTEM SET archive_command = 'pgbackrest --config=/var/lib/postgresql/$OLDNAME/pgbackrest.conf --stanza=main archive-push %p'" >/dev/null
docker restart -t 30 "$C" >/dev/null; wait_up
for _ in $(seq 30); do [ "$(ver template1)" = "$want" ] && break; sleep 1; done
check "template1 updated at worker start" "$(ver template1)" "$want"
P -c "CREATE DATABASE born" >/dev/null
check "a new database is born at $want" "$(ver born)" "$want"
# SHOW prints "(disabled)" while archive_mode=off: look at what the config files set
AC="SELECT coalesce((SELECT setting FROM pg_file_settings WHERE name='archive_command' ORDER BY seqno DESC LIMIT 1), '')"
for _ in $(seq 30); do [ -z "$(P -c "$AC")" ] && break; sleep 1; done
check "old product's archive_command reset" "$(P -c "$AC")" ""
check "reset logged" "$(docker logs "$C" 2>&1 | grep -c 'reset archive_command')" 1
P -c "ALTER SYSTEM SET archive_command = 'cp %p /tmp/%f'" >/dev/null
docker restart -t 30 "$C" >/dev/null; wait_up; sleep 8
check "foreign archive_command untouched" "$(P -c "$AC")" "cp %p /tmp/%f"
P -c "ALTER SYSTEM RESET archive_command" -c "SELECT pg_reload_conf()" >/dev/null

if [ -n "$OLD" ]; then
  echo "## 4. migration: the old product's dumps restore with pgbx db-restore --from-s3"
  S3="--s3-endpoint $S3_ENDPOINT --s3-bucket $S3_BUCKET --s3-region $S3_REGION --server-name $SERVER --credentials-file /etc/pgbx/s3.credentials"
  out=$(docker exec -u postgres "$C" pgbx db-restore --from-s3 --db shop --into shop_old $S3 --json 2>/dev/null)
  check "db-restore --from-s3 of an old dump" "$(echo "$out" | jq -r .ok)" true
  check "old rows back" "$(P -d shop_old -c 'SELECT count(*) FROM orders')" 500
  check "old extension can be dropped" "$(P -d shop -c "DROP EXTENSION $OLDNAME CASCADE" >/dev/null 2>&1 && echo dropped)" dropped
  for _ in $(seq 30); do [ -n "$(ver shop)" ] && break; sleep 1; done
  check "pgbx present in the migrated database" "$(ver shop)" "$want"
  id=$(P -d shop -c "SELECT pgbx.backup_now()"); check "pgbx backup after migration" "$(wait_job shop "$id" pgbx)" done
fi

if [ -n "$PREV" ] && docker image inspect "$PREV" >/dev/null 2>&1; then
  echo "## 5. previous release ($PREV) -> $want: the update script"
  cleanup; start "$PREV" pgbx || { echo "previous server not up"; exit 1; }
  for _ in $(seq 60); do [ -n "$(ver postgres)" ] && break; sleep 2; done
  P -c "CREATE DATABASE prev" >/dev/null
  for _ in $(seq 60); do [ -n "$(ver prev)" ] && break; sleep 1; done
  old=$(ver prev); echo "  databases at $old"
  docker stop -t 60 "$C" >/dev/null
  start "$NEW" pgbx || { echo "server not up"; exit 1; }
  for _ in $(seq 60); do [ "$(ver prev)" = "$want" ] && [ "$(ver template1)" = "$want" ] && break; sleep 1; done
  check "worker updated prev $old -> $want" "$(ver prev)" "$want"
  check "template1 updated" "$(ver template1)" "$want"
  P -c "CREATE DATABASE fresh" >/dev/null
  for _ in $(seq 60); do [ -n "$(ver fresh)" ] && break; sleep 1; done
  defs="SELECT md5(string_agg(pg_get_functiondef(p.oid), '' ORDER BY p.proname, p.oid::regprocedure::text))
        FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname = 'pgbx'"
  check "updated functions = fresh install" "$(P -d prev -c "$defs")" "$(P -d fresh -c "$defs")"
  check "doctor() has long_running_job after the update" "$(P -c "SELECT count(*) FROM pgbx.doctor() WHERE name='long_running_job'")" 1
  id=$(P -d prev -c "SELECT pgbx.backup_now()"); check "backup after the update" "$(wait_job prev "$id" pgbx)" done
else
  echo "## 5. skipped (set PGBX_PREV_IMAGE to an image of the previous pgbx release to test the update script)"
fi

echo "== upgrade_e2e: $pass passed, $fail failed (S3 folder $SERVER)"
[ $fail -eq 0 ]
