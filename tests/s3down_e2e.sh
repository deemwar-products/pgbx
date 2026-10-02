#!/usr/bin/env bash
# S3 unreachable (bash 3.2-safe): the worker's poll loop must never wait for S3 (known issue in 0.5: the retries of a
# job blocked the loop for ~2 minutes, so new databases got no extension). Throwaway container pgbx-s3down whose S3
# endpoint is a black hole (connections hang, nothing answers: the slowest failure mode).
#   - worker starts (its startup cleanup of orphaned uploads runs on a thread of its own)
#   - new databases get the extension within seconds while their first backups hang on S3
#   - the overview heartbeat keeps moving; pgbx jobs shows the stuck job; cancel stops it (at the S3 request timeout)
#   - Postgres stops promptly while a backup is stuck on S3
# IMAGE (pgbx:test). S3DOWN_KEEP=1 leaves the container running.
set -u
IMAGE=${IMAGE:-pgbx:test}; C=pgbx-s3down
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "  PASS $1"; pass=$((pass+1)); else echo "  FAIL $1 (got '$2', want '$3')"; fail=$((fail+1)); fi; }
P() { docker exec -u postgres "$C" psql -v ON_ERROR_STOP=1 -qAt "$@"; }
cleanup() { docker rm -fv "$C" >/dev/null 2>&1; }
trap '[ "${S3DOWN_KEEP:-0}" = 1 ] || cleanup' EXIT
cleanup
echo "== s3down_e2e image $IMAGE (S3 endpoint: a black hole)"
# 10.255.255.1 is not routed: TCP connects hang until their timeout, the worst case for anything waiting on S3
docker run -d --rm --name "$C" -e POSTGRES_PASSWORD=test-only-not-secret -e PGDATA=/var/lib/postgresql/data "$IMAGE" \
  sh -c 'mkdir -p /etc/pgbx && printf "access_key_id=x\nsecret_access_key=y\n" > /etc/pgbx/s3.credentials &&
         chown postgres /etc/pgbx/s3.credentials && chmod 600 /etc/pgbx/s3.credentials && exec docker-entrypoint.sh "$@"' -- \
  postgres -c shared_preload_libraries=pgbx -c pgbx.s3_endpoint=http://10.255.255.1:9000 -c pgbx.s3_bucket=none \
  -c pgbx.server_name=s3down -c pgbx.credentials_file=/etc/pgbx/s3.credentials -c pgbx.poll_seconds=2 >/dev/null
for _ in $(seq 120); do docker logs "$C" 2>&1 | grep -q "init process complete" && break; sleep 1; done
ok=0; for _ in $(seq 120); do if P -c "SELECT 1" >/dev/null 2>&1; then ok=$((ok+1)); [ $ok -ge 2 ] && break; else ok=0; fi; sleep 1; done
for _ in $(seq 30); do [ "$(P -c "SELECT count(*) FROM pg_extension WHERE extname='pgbx'" 2>/dev/null)" = 1 ] && break; sleep 1; done

echo "## new databases while S3 does not answer"
for db in a1 a2 a3; do
  t0=$(date +%s); P -c "CREATE DATABASE $db" >/dev/null
  for _ in $(seq 30); do [ "$(P -d $db -c "SELECT count(*) FROM pg_extension WHERE extname='pgbx'" 2>/dev/null)" = 1 ] && break; sleep 1; done
  took=$(( $(date +%s) - t0 ))
  check "database $db gets the extension within 15 s (${took}s)" "$([ $took -le 15 ] && echo yes || echo no)" yes
done
for _ in $(seq 20); do [ "$(P -c "SELECT count(*) FROM pgbx.server_queue WHERE state='running'")" -ge 1 ] && break; sleep 1; done
check "a first backup is running (stuck on S3, on its job thread)" "$(P -c "SELECT count(*) FROM pgbx.server_queue WHERE state='running'")" 1
s1=$(P -c "SELECT max(seen_at) FROM pgbx.server_overview"); sleep 5; s2=$(P -c "SELECT max(seen_at) FROM pgbx.server_overview")
check "the worker heartbeat keeps moving while a job waits on S3" "$([ "$s2" \> "$s1" ] && echo yes || echo no)" yes
check "every database reported in" "$(P -c "SELECT count(*) FROM pgbx.server_overview WHERE database IN ('a1','a2','a3')")" 3
r=$(docker exec -u postgres "$C" pgbx jobs --json)
check "pgbx jobs answers while S3 is down" "$(echo "$r" | grep -c '"state":"running"')" 1

echo "## cancel and shutdown with a job stuck on S3"
row=$(P -c "SELECT database||' '||job_id FROM pgbx.server_queue WHERE state='running' LIMIT 1"); db=${row% *}; id=${row#* }
P -d "$db" -c "SELECT pgbx.cancel($id)" >/dev/null
# a request to a black hole ends at the S3 request timeout (60 s); then the cancel takes effect, no retry starts
for _ in $(seq 80); do s=$(P -d "$db" -c "SELECT state FROM pgbx.history WHERE id=$id"); [ "$s" = cancelled ] && break; sleep 1; done
check "cancel stops the stuck backup (within the 60 s S3 request timeout)" "$s" cancelled
for _ in $(seq 20); do [ "$(P -c "SELECT count(*) FROM pgbx.server_queue WHERE state='running'")" -ge 1 ] && break; sleep 1; done
t0=$(date +%s); docker stop -t 60 "$C" >/dev/null; took=$(( $(date +%s) - t0 ))
check "Postgres stopped within 10 s while a backup waits on S3 (${took}s)" "$([ $took -lt 10 ] && echo yes || echo no)" yes

echo "== s3down_e2e: $pass passed, $fail failed"
[ $fail -eq 0 ]
