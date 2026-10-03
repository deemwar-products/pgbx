#!/usr/bin/env bash
# What client-side encryption costs end to end (bash 3.2-safe): a ~1.3 GB database backed up to a local S3 (RustFS)
# with encryption off and on, ALTERNATING so drift hits both alike, then restored once each way. Throwaway containers
# pgbx-encb-* (removed at the end, with their volumes). RUNS=${RUNS:-3} per mode, ROWS=${ROWS:-9000000},
# COMPRESSION=${COMPRESSION:-auto} (pgbx.dump_compression; 'none' shows the cost on raw dump bytes). IMAGE (pgbx:test).
set -u
IMAGE=${IMAGE:-pgbx:test}; RUNS=${RUNS:-3}; ROWS=${ROWS:-9000000}; COMP=${COMPRESSION:-auto}
NET=pgbx-encb-net; S3=pgbx-encb-s3; DB=pgbx-encb-db
P() { docker exec -u postgres "$DB" psql -v ON_ERROR_STOP=1 -qAt "$@"; }
cleanup() { docker rm -fv "$DB" "$S3" >/dev/null 2>&1; docker network rm "$NET" >/dev/null 2>&1; [ -n "${W:-}" ] && rm -rf "$W"; }
trap cleanup EXIT
cleanup; W=$(mktemp -d)
AK="encb$RANDOM$RANDOM"; SK=$(LC_ALL=C tr -dc 'a-z0-9' </dev/urandom | head -c 32)
printf 'access_key_id=%s\nsecret_access_key=%s\n' "$AK" "$SK" > "$W/s3.credentials"
head -c 32 /dev/urandom | base64 > "$W/backup.key"
docker network create "$NET" >/dev/null
docker run -d --rm --name "$S3" --network "$NET" -e RUSTFS_ACCESS_KEY="$AK" -e RUSTFS_SECRET_KEY="$SK" rustfs/rustfs >/dev/null
for _ in $(seq 30); do docker run --rm --network "$NET" -e AWS_ACCESS_KEY_ID="$AK" -e AWS_SECRET_ACCESS_KEY="$SK" -e AWS_DEFAULT_REGION=us-east-1 \
  amazon/aws-cli --endpoint-url "http://$S3:9000" s3 mb s3://encb 2>&1 | grep -qE 'make_bucket|BucketAlready' && break; sleep 1; done
docker run -d --rm --name "$DB" --network "$NET" -e POSTGRES_PASSWORD=test-only-not-secret -e PGDATA=/var/lib/postgresql/data "$IMAGE" \
  postgres -c shared_preload_libraries=pgbx -c pgbx.s3_endpoint="http://$S3:9000" -c pgbx.s3_bucket=encb -c pgbx.server_name=encb \
  -c pgbx.credentials_file=/etc/pgbx/s3.credentials -c pgbx.poll_seconds=1 -c pgbx.dump_compression="$COMP" -c max_wal_size=4GB >/dev/null
docker exec -u root "$DB" mkdir -p /etc/pgbx
for f in s3.credentials backup.key; do docker cp "$W/$f" "$DB:/etc/pgbx/$f" >/dev/null; docker exec -u root "$DB" sh -c "chown postgres /etc/pgbx/$f && chmod 600 /etc/pgbx/$f"; done
for _ in $(seq 120); do docker logs "$DB" 2>&1 | grep -q "init process complete" && break; sleep 1; done
for _ in $(seq 60); do P -c "SELECT 1" >/dev/null 2>&1 && break; sleep 1; done
P -c "CREATE DATABASE big" >/dev/null
for _ in $(seq 30); do P -d big -c "SELECT pgbx.pause('bench')" >/dev/null 2>&1 && break; sleep 1; done
P -d big -c "CREATE TABLE t AS SELECT g AS id, md5(g::text)||md5((g+1)::text) AS a, md5((g*3)::text) AS b, now() AS ts FROM generate_series(1,$ROWS) g"
echo "== encryption_bench: $(P -c "SELECT pg_size_pretty(pg_database_size('big'))") database, dump_compression=$COMP, $(docker exec "$DB" nproc) CPUs, $RUNS runs per mode"
job() { for _ in $(seq 1800); do s=$(P -d big -c "SELECT state||'|'||coalesce(bytes,0)||'|'||extract(epoch FROM finished-started)::numeric(10,2) FROM pgbx.history WHERE id=$1"); case "$s" in done*|failed*) echo "$s"; return;; esac; sleep 0.5; done; echo timeout; }
setkey() { P -c "ALTER SYSTEM SET pgbx.encryption_key_file = '$1'" -c "SELECT pg_reload_conf()" >/dev/null; sleep 2; }
: > "$W/plain"; : > "$W/enc"
for i in $(seq "$RUNS"); do
  for mode in plain enc; do
    if [ $mode = enc ]; then setkey /etc/pgbx/backup.key; else setkey ''; fi
    r=$(job "$(P -d big -c "SELECT pgbx.backup_now()" 2>/dev/null)"); echo "${r##*|}" >> "$W/$mode"
    echo "  run $i $mode: $r (state|bytes|seconds)"
  done
done
med() { sort -n "$1" | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}'; }
mp=$(med "$W/plain"); me=$(med "$W/enc")
echo "  backup median: plain ${mp}s, encrypted ${me}s ($(awk -v a="$mp" -v b="$me" 'BEGIN{printf "%+.1f %%", (b-a)/a*100}'))"
setkey /etc/pgbx/backup.key   # the worker needs the key to read the encrypted dump; plain dumps pass through
for mode in plain enc; do
  k=$(P -d big -c "SELECT s3_key FROM pgbx.history WHERE kind='backup' AND state='done' AND params->>'encrypted' = '$([ $mode = enc ] && echo true || echo false)' ORDER BY id DESC LIMIT 1")
  at=$(P -d big -c "SELECT to_char(finished AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS')||'+00' FROM pgbx.history WHERE s3_key='$k'")
  r=$(job "$(P -d big -c "SELECT pgbx.restore('r_$mode', '$at')" 2>/dev/null)")
  echo "  restore $mode ($k): $r; rows $(P -d "r_$mode" -c "SELECT count(*) FROM t" 2>/dev/null)"
done
echo "== encryption_bench done"
