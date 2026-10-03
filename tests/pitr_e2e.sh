#!/usr/bin/env bash
# Point-in-time restore end to end, in throwaway containers (never the compose stacks):
#   network pgbx-pitr-net-<sfx>, local S3 (RustFS) pgbx-pitr-s3-<sfx>, Postgres + pgbx pgbx-pitr-db-<sfx>.
# PG_MAJOR=13..18 (default 16) must match the image PITR_IMAGE (default pgbx:test, built by tests/run_all.sh with
#   that PG_MAJOR). PGBX_PREV_IMAGE (default pgbx:0.5.0-prev) supplies pgbx--0.5.0.sql for the upgrade check.
# PITR_KEEP=1 leaves the containers running for inspection.
#
#   1  setup pitr refuses a foreign archive_command, then enables PITR (restart once)
#   2  first base backup, rows A; rows B; T; rows C -> restore --time T into a copy on another port: A+B, no C;
#      the copy does not archive (archive_mode off, pgbx.pitr off, own server_name)
#   3  wal-push idempotency (same file = ok) and checksum-mismatch refusal; wal-get of a missing segment = exit 1
#   4  S3 down -> backlog -> drops past pgbx.wal_queue_max -> gap row + alert -> no false 'recovered' ->
#      a new database still gets the extension at once (the worker never waits for S3) ->
#      restore inside the gap refused -> S3 back -> healing base backup -> gap closed -> restore latest (S3 flags)
#   5  pitr_status / doctor rows / backup_now superuser-only
#   6  upgrade 0.5.0 -> 0.6.0 gives exactly the fresh 0.6.0 catalog
#   7  Postgres stops promptly while a base backup runs
set -u
PG=${PG_MAJOR:-16}
IMAGE=${PITR_IMAGE:-pgbx:test}
OLD=${PGBX_PREV_IMAGE:-pgbx:0.5.0-prev}
SFX=${PITR_SUFFIX:-$PG}
NET=pgbx-pitr-net-$SFX; S3=pgbx-pitr-s3-$SFX; DB=pgbx-pitr-db-$SFX
BUCKET=pitr; SERVER=pitr-e2e-$(date -u +%H%M%S)
BIN=/usr/lib/postgresql/$PG/bin
CONF=/var/lib/postgresql/pgbx/pgbx-wal.conf
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "  PASS $1"; pass=$((pass+1)); else echo "  FAIL $1 (got '$2', want '$3')"; fail=$((fail+1)); fi; }
P() { docker exec -u postgres "$DB" psql -v ON_ERROR_STOP=1 -qAt "$@"; }
P5() { docker exec -u postgres "$DB" psql -h /tmp -p 5433 -qAt "$@"; }
X() { docker exec -u postgres "$DB" "$@"; }
cleanup() { docker rm -fv "$DB" "$S3" >/dev/null 2>&1; docker network rm "$NET" >/dev/null 2>&1; [ -n "${TMPD:-}" ] && rm -rf "$TMPD"; }
trap '[ "${PITR_KEEP:-0}" = 1 ] || cleanup' EXIT
cleanup
TMPD=$(mktemp -d)
AK="pitr$RANDOM$RANDOM"; SK=$(LC_ALL=C tr -dc 'a-z0-9' </dev/urandom | head -c 32)
printf 'access_key_id=%s\nsecret_access_key=%s\n' "$AK" "$SK" > "$TMPD/s3.credentials"; chmod 644 "$TMPD/s3.credentials"

echo "== pitr_e2e PG $PG image $IMAGE (server folder $SERVER)"
docker network create "$NET" >/dev/null
docker run -d --name "$S3" --network "$NET" -e RUSTFS_ACCESS_KEY="$AK" -e RUSTFS_SECRET_KEY="$SK" rustfs/rustfs >/dev/null
mkbucket() {
  for _ in $(seq 30); do
    docker run --rm --network "$NET" -e AWS_ACCESS_KEY_ID="$AK" -e AWS_SECRET_ACCESS_KEY="$SK" -e AWS_DEFAULT_REGION=us-east-1 \
      amazon/aws-cli --endpoint-url "http://$S3:9000" s3 mb "s3://$BUCKET" 2>&1 | grep -qE 'make_bucket|BucketAlready' && return 0
    sleep 1
  done; return 1
}
mkbucket || { echo "  FAIL local S3 did not come up"; exit 1; }
docker run -d --name "$DB" --network "$NET" -e POSTGRES_PASSWORD=test-only-not-secret -e PGDATA=/var/lib/postgresql/data \
  -v "$TMPD/s3.credentials:/etc/pgbx/s3.credentials.src:ro" "$IMAGE" \
  sh -c 'mkdir -p /etc/pgbx && cp /etc/pgbx/s3.credentials.src /etc/pgbx/s3.credentials && chown postgres /etc/pgbx/s3.credentials && chmod 600 /etc/pgbx/s3.credentials && exec docker-entrypoint.sh "$@"' -- \
  postgres -c shared_preload_libraries=pgbx -c "pgbx.s3_endpoint=http://$S3:9000" -c pgbx.s3_bucket=$BUCKET \
  -c pgbx.server_name=$SERVER -c pgbx.credentials_file=/etc/pgbx/s3.credentials -c pgbx.poll_seconds=2 \
  -c "pgbx.alert_command=cat >> /var/lib/postgresql/alerts.log; echo >> /var/lib/postgresql/alerts.log" \
  -c pgbx.wal_alert_after=10s >/dev/null
waitpg() {
  for _ in $(seq 120); do docker logs "$DB" 2>&1 | grep -q "init process complete" && break; sleep 1; done
  ok=0; for _ in $(seq 120); do if P -c "SELECT 1" >/dev/null 2>&1; then ok=$((ok+1)); [ $ok -ge 2 ] && return 0; else ok=0; fi; sleep 1; done; return 1
}
waitpg || { echo "  FAIL Postgres did not start"; docker logs "$DB" 2>&1 | tail -20; exit 1; }
sleep 3

echo "## 1. pgbx setup pitr"
P -c "ALTER SYSTEM SET archive_command = 'cp %p /tmp/%f'" -c "SELECT pg_reload_conf()" >/dev/null
sleep 2   # (SHOW archive_command says "(disabled)" while archive_mode is off)
r=$(X pgbx setup pitr --yes --json)
check "foreign archive_command refused" "$(echo "$r" | grep -c 'will not replace')" 1
[ "$(echo "$r" | grep -c 'will not replace')" = 1 ] || echo "    $r / $(P -c 'SHOW archive_command')"
P -c "ALTER SYSTEM RESET archive_command" -c "SELECT pg_reload_conf()" >/dev/null; sleep 1
r=$(X pgbx setup pitr --json); check "setup without --yes only shows the plan" "$(echo "$r" | grep -c '"guarded')" 1
r=$(X pgbx setup --pitr --yes --json)
check "setup --pitr (alias of setup pitr) ok + restart needed" "$(echo "$r" | grep -c '"restart_needed":true')" 1
docker restart "$DB" >/dev/null; waitpg
check "archive_command is pgbx wal-push" "$(P -c "SHOW archive_command")" "/usr/local/bin/pgbx wal-push %p"
check "archive_mode on" "$(P -c "SHOW archive_mode")" on

echo "## 2. base backup, rows, restore to T into a copy"
P -c "CREATE TABLE t (id int PRIMARY KEY, batch text, at timestamptz DEFAULT clock_timestamp())" \
  -c "INSERT INTO t SELECT g, 'A' FROM generate_series(1,1000) g"
for _ in $(seq 120); do s=$(P -c "SELECT state FROM pgbx.history WHERE kind='base_backup' ORDER BY id LIMIT 1"); [ "$s" = done ] || [ "$s" = failed ] && break; sleep 1; done
check "first base backup (trigger first) done" "$(P -c "SELECT state||'/'||trigger FROM pgbx.history WHERE kind='base_backup' ORDER BY id LIMIT 1")" "done/first"
[ "$s" = done ] || P -c "SELECT error FROM pgbx.history WHERE kind='base_backup' ORDER BY id LIMIT 1"
P -c "INSERT INTO t SELECT g, 'B' FROM generate_series(1001,2000) g" -c "SELECT pg_switch_wal()" >/dev/null
sleep 2; T=$(P -c "SELECT to_char(clock_timestamp() AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS.US')||'+00'"); sleep 2
P -c "INSERT INTO t SELECT g, 'C' FROM generate_series(2001,3000) g" -c "SELECT pg_switch_wal()" >/dev/null
for _ in $(seq 60); do [ "$(P -c "SELECT count(*) FROM pg_ls_archive_statusdir() WHERE name LIKE '%.ready'")" = 0 ] && break; sleep 1; done
check "WAL archived (no .ready left)" "$(P -c "SELECT count(*) FROM pg_ls_archive_statusdir() WHERE name LIKE '%.ready'")" 0
r=$(X pgbx pitr restore --time "$T" --target /tmp/r --conf $CONF --json)
check "pitr restore --time T ok" "$(echo "$r" | grep -c '"ok":true')" 1
[ "$(echo "$r" | grep -c '"ok":true')" = 1 ] || echo "    $r"
r2=$(X pgbx pitr restore --time "$T" --target /tmp/r --conf $CONF --json)
check "restore into a non-empty dir refused" "$(echo "$r2" | grep -c 'not empty')" 1
r3=$(X pgbx pitr restore --time "$T" --target /var/lib/postgresql/data --yes-replace-whole-server --conf $CONF --json)
check "in-place restore refused while Postgres runs" "$(echo "$r3" | grep -c 'is running')" 1
r4=$(X pgbx pitr restore --time "2026-01-01 00:00:00" --target /tmp/rx --conf $CONF --json)
check "time without UTC offset refused" "$(echo "$r4" | grep -c 'no UTC offset')" 1
X $BIN/pg_ctl -D /tmp/r -o "-p 5433 -k /tmp" -l /tmp/r/pgbx-restore/recovery.log -w -t 180 start >/dev/null
for _ in $(seq 120); do [ "$(P5 -c "SELECT pg_is_in_recovery()" 2>/dev/null)" = f ] && break; sleep 1; done
check "copy promoted" "$(P5 -c "SELECT pg_is_in_recovery()")" f
check "rows as of T present (A+B)" "$(P5 -c "SELECT count(*) FROM t WHERE batch IN ('A','B')")" 2000
check "rows after T absent (C)" "$(P5 -c "SELECT count(*) FROM t WHERE batch = 'C'")" 0
check "copy: archive_mode off" "$(P5 -c "SHOW archive_mode")" off
check "copy: pgbx.pitr off" "$(P5 -c "SHOW pgbx.pitr")" off
check "copy: own server_name" "$(P5 -c "SHOW pgbx.server_name" | grep -c -- "-pitr-copy-")" 1
X $BIN/pg_ctl -D /tmp/r -m fast stop >/dev/null; docker exec "$DB" rm -rf /tmp/r

echo "## 3. wal-push idempotency, checksum refusal, wal-get missing"
seg=$(P -c "SELECT last_archived_wal FROM pg_stat_archiver")
docker exec -u postgres "$DB" sh -c "mkdir -p /tmp/w/pg_wal/archive_status /tmp/w/pgbx && sed -e 's/^async=.*/async=off/' -e 's#^work_dir=.*#work_dir=/tmp/w/pgbx#' $CONF > /tmp/w/pgbx/pgbx-wal.conf"
X pgbx wal-get "$seg" "pg_wal/$seg" --conf /tmp/w/pgbx/pgbx-wal.conf >/dev/null 2>&1 || true
docker exec -u postgres -w /tmp/w "$DB" pgbx wal-get "$seg" "pg_wal/$seg" --conf /tmp/w/pgbx/pgbx-wal.conf; rc=$?
check "wal-get archived segment exit 0" "$rc" 0
docker exec -u postgres -w /tmp/w "$DB" pgbx wal-push "pg_wal/$seg" --conf /tmp/w/pgbx/pgbx-wal.conf; rc=$?
check "wal-push same file again = success (idempotent)" "$rc" 0
docker exec -u postgres -w /tmp/w "$DB" sh -c "printf 'X' | dd of=pg_wal/$seg bs=1 seek=100000 conv=notrunc 2>/dev/null"
out=$(docker exec -u postgres -w /tmp/w "$DB" pgbx wal-push "pg_wal/$seg" --conf /tmp/w/pgbx/pgbx-wal.conf 2>&1); rc=$?
check "wal-push changed file refused (exit 1)" "$rc" 1
check "refusal names the checksum conflict" "$(echo "$out" | grep -c 'DIFFERENT checksum')" 1
docker exec -u postgres -w /tmp/w "$DB" pgbx wal-get 0000000100000000000000FE pg_wal/RECOVERYXLOG --conf /tmp/w/pgbx/pgbx-wal.conf; rc=$?
check "wal-get missing segment exit 1" "$rc" 1
docker exec -u postgres -w /tmp/w "$DB" pgbx wal-get 00000063.history pg_wal/RECOVERYHISTORY --conf /tmp/w/pgbx/pgbx-wal.conf; rc=$?
check "wal-get missing history exit 1" "$rc" 1
docker exec "$DB" rm -rf /tmp/w

echo "## 4. S3 down -> backlog -> gap -> heal"
P -c "ALTER SYSTEM SET pgbx.wal_queue_max = '64MB'" -c "SELECT pg_reload_conf()" >/dev/null; sleep 5
docker stop "$S3" >/dev/null; echo "  S3 stopped"
for i in $(seq 10); do P -c "INSERT INTO t SELECT g, 'D' FROM generate_series($((3000+i*100+1)),$((3000+i*100+100))) g" -c "SELECT pg_switch_wal()" >/dev/null; done
for _ in $(seq 180); do [ "$(P -c "SELECT count(*) FROM pgbx.history WHERE kind='wal_gap' AND state='running'")" = 1 ] && break; sleep 1; done
check "gap recorded (wal_gap running)" "$(P -c "SELECT count(*) FROM pgbx.history WHERE kind='wal_gap' AND state='running'")" 1
check "drops logged by wal-push" "$(docker exec "$DB" sh -c 'test -s /var/lib/postgresql/pgbx/wal-drops.log && echo yes')" yes
sleep 3
check "wal_gap alert sent" "$(docker exec "$DB" grep -c '"kind":"wal_gap"' /var/lib/postgresql/alerts.log)" 1
check "pitr_status says gap" "$(P -c "SELECT state LIKE 'gap:%' FROM pgbx.pitr_status()")" t
GAPT=$(P -c "SELECT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS')||'+00'")
# the worker never waits for S3 on its poll loop: a new database gets the extension (and a first backup that retries
# on its own thread) within seconds while S3 is down
t0=$(date +%s); P -c "CREATE DATABASE s3down" >/dev/null
for _ in $(seq 30); do [ "$(P -d s3down -c "SELECT count(*) FROM pg_extension WHERE extname='pgbx'" 2>/dev/null)" = 1 ] && break; sleep 1; done
took=$(( $(date +%s) - t0 ))
check "S3 down: a new database gets the extension within 15 s (${took}s)" "$([ $took -le 15 ] && echo yes || echo no)" yes
for _ in $(seq 20); do [ -n "$(P -d s3down -c "SELECT state FROM pgbx.history WHERE kind='backup' AND state IN ('running','failed')" 2>/dev/null)" ] && break; sleep 1; done
check "S3 down: its first backup runs (retrying on its job thread) or failed, never stuck queued" \
  "$(P -d s3down -c "SELECT count(*) FROM pgbx.history WHERE kind='backup' AND state IN ('running','failed')")" 1
P -d s3down -c "SELECT pgbx.cancel(id) FROM pgbx.history WHERE kind='backup' AND state='running'" >/dev/null 2>&1
P -d s3down -c "SELECT pgbx.pause('s3down test')" >/dev/null 2>&1   # its slot is free again for the healing base backup
sleep 20
check "no false 'recovered' alert while S3 is down" "$(docker exec "$DB" grep -c 'wal_archive_recovered' /var/lib/postgresql/alerts.log)" 0
# the gap list is in S3 only once S3 is back; restore refusal is checked after it is published
docker start "$S3" >/dev/null; echo "  S3 back"
sleep 3; P -c "INSERT INTO t VALUES (9001, 'E')" -c "SELECT pg_switch_wal()" >/dev/null
for _ in $(seq 180); do [ "$(P -c "SELECT state FROM pgbx.history WHERE kind='wal_gap' ORDER BY id DESC LIMIT 1")" = done ] && break; sleep 2; done
check "healing base backup queued by the gap" "$(P -c "SELECT count(*) FROM pgbx.history WHERE kind='base_backup' AND trigger='wal_gap' AND state='done'")" 1
check "gap closed with healed_at" "$(P -c "SELECT state||'/'||(params ? 'healed_at') FROM pgbx.history WHERE kind='wal_gap' ORDER BY id DESC LIMIT 1")" "done/true"
sleep 6
F="--s3-endpoint http://$S3:9000 --s3-bucket $BUCKET --server-name $SERVER --credentials-file /etc/pgbx/s3.credentials"
r=$(X pgbx pitr restore --time "$GAPT" --target /tmp/rg $F --json)
check "restore inside the gap refused (S3 flags, no conf)" "$(echo "$r" | grep -c 'inside WAL gap')" 1
P -c "INSERT INTO t VALUES (9002, 'F')" -c "SELECT pg_switch_wal()" >/dev/null; sleep 4
r=$(X pgbx pitr restore --time latest --target /tmp/r2 $F --json)
check "restore latest after heal ok" "$(echo "$r" | grep -c '"ok":true')" 1
X $BIN/pg_ctl -D /tmp/r2 -o "-p 5433 -k /tmp" -l /tmp/r2/pgbx-restore/recovery.log -w -t 180 start >/dev/null
for _ in $(seq 120); do [ "$(P5 -c "SELECT pg_is_in_recovery()" 2>/dev/null)" = f ] && break; sleep 1; done
check "latest restore has the row written after the heal" "$(P5 -c "SELECT count(*) FROM t WHERE id = 9002")" 1
X $BIN/pg_ctl -D /tmp/r2 -m fast stop >/dev/null; docker exec "$DB" rm -rf /tmp/r2

echo "## 5. status, doctor, privileges"
check "pitr_status active" "$(P -c "SELECT state FROM pgbx.pitr_status()")" active
check "restorable_from set" "$(P -c "SELECT restorable_from IS NOT NULL FROM pgbx.pitr_status()")" t
check "doctor pitr rows ok" "$(P -c "SELECT string_agg(name||'='||ok, ',' ORDER BY name) FROM pgbx.doctor() WHERE name LIKE 'pitr%'")" "pitr archiving=true,pitr base backups=true,pitr gaps=true"
P -c "CREATE ROLE pitr_admin LOGIN IN ROLE pgbx_admin" >/dev/null
check "backup_now refused for a non-superuser" "$(P -U pitr_admin -d postgres -c "SELECT pgbx.pitr_backup_now()" 2>&1 | grep -c 'permission denied\|needs a superuser')" 1
check "viewer may read pitr_status" "$(P -U pitr_admin -d postgres -c "SELECT enabled FROM pgbx.pitr_status()")" t
check "per-database status ignores PITR rows (never running/failing from them)" "$(P -c "SELECT state NOT IN ('running','failing') FROM pgbx.status()")" t
r=$(X pgbx pitr status --json); check "pgbx pitr status" "$(echo "$r" | grep -c '"state":"active"')" 1
r=$(X pgbx pitr list $F --json); check "pgbx pitr list from S3" "$(echo "$r" | grep -c '"base_backups":\[{')" 1

echo "## 6. upgrade 0.5.0 -> 0.6.0 == fresh 0.6.0"
EXT=/usr/share/postgresql/$PG/extension
# the 0.5.0 install script is the same SQL for every major; take whichever major the old image carries
if docker run --rm --entrypoint sh "$OLD" -c 'cat $(ls /usr/share/postgresql/*/extension/pgbx--0.5.0.sql | head -1)' > "$TMPD/old.sql" 2>/dev/null && [ -s "$TMPD/old.sql" ]; then
  docker cp "$TMPD/old.sql" "$DB:$EXT/pgbx--0.5.0.sql"
  cat > "$TMPD/cat.sql" <<'SQL'
SELECT 'F ' || p.proname || '(' || pg_get_function_identity_arguments(p.oid) || ') ' || md5(pg_get_functiondef(p.oid))
       || ' acl=' || coalesce((SELECT string_agg(a::text, ',' ORDER BY a::text) FROM unnest(p.proacl) a), '-')
  FROM pg_proc p WHERE p.pronamespace = 'pgbx'::regnamespace
UNION ALL SELECT 'C ' || c.relname || '.' || a.attname || ' ' || format_type(a.atttypid, a.atttypmod) || ' ' || a.attnotnull
       || ' ' || coalesce(pg_get_expr(d.adbin, d.adrelid), '-')
  FROM pg_class c JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
  LEFT JOIN pg_attrdef d ON d.adrelid = c.oid AND d.adnum = a.attnum
 WHERE c.relnamespace = 'pgbx'::regnamespace
UNION ALL SELECT 'K ' || conrelid::regclass::text || ' ' || conname || ' ' || pg_get_constraintdef(oid) FROM pg_constraint WHERE connamespace = 'pgbx'::regnamespace
UNION ALL SELECT 'R ' || c.relname || ' ' || c.relkind::text || ' acl=' || coalesce((SELECT string_agg(a::text, ',' ORDER BY a::text) FROM unnest(c.relacl) a), '-')
  FROM pg_class c WHERE c.relnamespace = 'pgbx'::regnamespace
UNION ALL SELECT 'V ' || viewname || ' ' || md5(definition) FROM pg_views WHERE schemaname = 'pgbx'
UNION ALL SELECT 'I ' || indexname || ' ' || indexdef FROM pg_indexes WHERE schemaname = 'pgbx'
ORDER BY 1;
SQL
  docker cp "$TMPD/cat.sql" "$DB:/tmp/cat.sql"
  P -c "CREATE DATABASE up_old TEMPLATE template0" -c "CREATE DATABASE up_new TEMPLATE template0"
  # the worker installs the current version in every new database within a poll: replace it with 0.5.0 in ONE
  # transaction (reading the version inside it), then update; if the worker updated it first, that was this script too
  v=$(P -d up_old -c "BEGIN" -c "DROP EXTENSION IF EXISTS pgbx" -c "CREATE EXTENSION pgbx VERSION '0.5.0'" \
        -c "SELECT extversion FROM pg_extension WHERE extname='pgbx'" -c "COMMIT" 2>/dev/null | grep -x '[0-9.]*')
  P -d up_old -c "ALTER EXTENSION pgbx UPDATE TO '0.6.0'" >/dev/null 2>&1
  P -d up_new -c "CREATE EXTENSION IF NOT EXISTS pgbx" >/dev/null
  check "old database started at 0.5.0" "$v" 0.5.0
  P -d up_old -f /tmp/cat.sql > "$TMPD/old.cat"; P -d up_new -f /tmp/cat.sql > "$TMPD/new.cat"
  check "catalog listing is not empty" "$([ "$(wc -l < "$TMPD/new.cat")" -gt 50 ] && echo yes || echo no)" yes
  if diff "$TMPD/old.cat" "$TMPD/new.cat" > "$TMPD/cat.diff"; then d=same; else d=different; sed -n 1,20p "$TMPD/cat.diff"; fi
  check "upgraded catalog == fresh catalog ($(wc -l < "$TMPD/new.cat" | tr -d ' ') objects)" "$d" same
else
  echo "  SKIP upgrade check: no pgbx--0.5.0.sql in $OLD"
fi

echo "## 7. a base backup is a job: pgbx jobs shows it, cancel stops it, shutdown does not wait for it"
P -c "CREATE TABLE big AS SELECT g, md5(g::text) m FROM generate_series(1,3000000) g" >/dev/null
nbb() { X pgbx pitr list --conf $CONF --json | grep -o '"label":"[0-9TZ]*"' | sort -u | wc -l | tr -d ' '; }
n0=$(nbb)
id=$(P -c "SELECT pgbx.pitr_backup_now()")
for _ in $(seq 60); do [ "$(P -c "SELECT state FROM pgbx.history WHERE id=$id")" = running ] && break; sleep 0.2; done
docker stop "$S3" >/dev/null   # its upload now waits on S3, so it is still running however fast this machine is
check "pgbx jobs lists the running base backup" "$(X pgbx jobs --json | grep -o '"kind":"base_backup"[^}]*"state":"running"' | wc -l | tr -d ' ')" 1
r=$(X pgbx jobs cancel "$id" --yes --json)
for _ in $(seq 60); do s=$(P -c "SELECT state FROM pgbx.history WHERE id=$id"); [ "$s" = cancelled ] && break; sleep 0.5; done
check "pgbx jobs cancel stops a running base backup (S3 not answering)" "$s" cancelled
[ "$s" = cancelled ] || P -c "SELECT state, error, params FROM pgbx.history WHERE id=$id"
docker start "$S3" >/dev/null; for _ in $(seq 30); do [ -n "$(nbb 2>/dev/null)" ] && [ "$(nbb)" -ge 1 ] && break; sleep 1; done
check "the cancelled base backup never became one (no new backup.json in S3)" "$(nbb)" "$n0"
P -c "SELECT pgbx.pitr_backup_now()" >/dev/null
for _ in $(seq 30); do [ "$(P -c "SELECT count(*) FROM pgbx.history WHERE kind='base_backup' AND state='running'")" = 1 ] && break; sleep 0.5; done
t0=$(date +%s); docker stop -t 60 "$DB" >/dev/null; took=$(( $(date +%s) - t0 ))
check "Postgres stopped within 10 s during a base backup (${took}s)" "$([ $took -lt 10 ] && echo yes || echo no)" yes
# a killed base backup must never leave pg_basebackup to the postmaster (PID 1 here): an unknown child that dies by a
# signal makes it restart the whole server
check "no server process died by a signal in the whole run (no crash restart)" "$(docker logs "$DB" 2>&1 | grep -c 'terminated by signal')" 0
check "clean shutdown" "$(docker logs "$DB" 2>&1 | tail -3 | grep -c 'database system is shut down')" 1

echo "== pitr_e2e PG $PG: $pass passed, $fail failed"
[ $fail -eq 0 ]
