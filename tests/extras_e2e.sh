#!/usr/bin/env bash
# pgbx 0.6 extras end to end (bash 3.2-safe), in throwaway containers (never the compose stacks):
#   network pgbx-x6-net, a local S3 (RustFS) pgbx-x6-s3, a webhook sink pgbx-x6-hook, Postgres + pgbx pgbx-x6-pg
#   (encryption on, notifications to the sink) and pgbx-x6-pg2 (a NEW server: restores from S3 onto it).
#   1 encryption: encrypted dumps, history says so, the S3 object is ciphertext; older unencrypted dumps still restore
#   2 roles: a roles file next to every dump; restore(with_roles) on the same server keeps owners and never touches
#     existing roles; --from-s3 --with-roles on a new server creates only the missing referenced roles (idempotent)
#   3 wrong / missing key fail loudly; pgbx decrypt turns a downloaded object back into a pg_restore-able dump
#   4 notifications: one webhook message per incident, one "recovered", no URL in the body
#   5 GFS retention: set_retention(gfs => ...), status(), the max_days_limit refusal; pgbx retention --gfs
#   6 metrics: pgbx metrics and GET /metrics on pgbx ui
# IMAGE (pgbx:test, built by tests/run_all.sh). PGBX_X6_KEEP=1 leaves the containers running.
set -u
IMAGE=${IMAGE:-pgbx:test}
NET=pgbx-x6-net; S3=pgbx-x6-s3; HOOK=pgbx-x6-hook; PG1=pgbx-x6-pg; PG2=pgbx-x6-pg2
BUCKET=extras; SERVER="x6-$(date -u +%Y%m%d%H%M%S)"
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "  PASS $1"; pass=$((pass+1)); else echo "  FAIL $1 (got '$2', want '$3')"; fail=$((fail+1)); fi; }
has() { case "$2" in *"$3"*) echo "  PASS $1"; pass=$((pass+1));; *) echo "  FAIL $1 ('$3' not in: $(echo "$2" | head -c 400))"; fail=$((fail+1));; esac; }
P() { local c=$1; shift; docker exec -u postgres "$c" psql -v ON_ERROR_STOP=1 -qAt "$@"; }
cleanup() { docker rm -fv "$PG1" "$PG2" "$S3" "$HOOK" >/dev/null 2>&1; docker network rm "$NET" >/dev/null 2>&1; [ -n "${WORK:-}" ] && rm -rf "$WORK"; }
trap '[ "${PGBX_X6_KEEP:-0}" = 1 ] || cleanup' EXIT
cleanup
WORK=$(mktemp -d)
AK="x6$RANDOM$RANDOM"; SK=$(LC_ALL=C tr -dc 'a-z0-9' </dev/urandom | head -c 32)
printf 'access_key_id=%s\nsecret_access_key=%s\n' "$AK" "$SK" > "$WORK/s3.credentials"
head -c 32 /dev/urandom | base64 > "$WORK/backup.key"
head -c 32 /dev/urandom | base64 > "$WORK/wrong.key"
printf 'webhook.hook.url=http://%s:8080/hook\n' "$HOOK" > "$WORK/notify.secrets"
AWS() { docker run --rm --network "$NET" -v "$WORK:/w" -e AWS_ACCESS_KEY_ID="$AK" -e AWS_SECRET_ACCESS_KEY="$SK" \
          -e AWS_DEFAULT_REGION=us-east-1 amazon/aws-cli --endpoint-url "http://$S3:9000" "$@"; }

echo "== extras_e2e image $IMAGE (server folder $SERVER)"
docker network create "$NET" >/dev/null
docker run -d --rm --name "$S3" --network "$NET" -e RUSTFS_ACCESS_KEY="$AK" -e RUSTFS_SECRET_KEY="$SK" rustfs/rustfs >/dev/null
for _ in $(seq 30); do AWS s3 mb "s3://$BUCKET" 2>&1 | grep -qE 'make_bucket|BucketAlready' && break; sleep 1; done
# webhook sink: every POST body becomes one line of /w/hook.log
cat > "$WORK/sink.py" <<'PY'
import http.server
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        b = self.rfile.read(int(self.headers.get('Content-Length', 0)))
        open('/w/hook.log', 'ab').write(b + b'\n'); self.send_response(200); self.end_headers()
    def log_message(self, *a): pass
http.server.HTTPServer(('0.0.0.0', 8080), H).serve_forever()
PY
docker run -d --rm --name "$HOOK" --network "$NET" -v "$WORK:/w" python:3.11-slim python3 /w/sink.py >/dev/null
put() { # container src dest mode
  docker cp "$2" "$1:$3" >/dev/null && docker exec -u root "$1" sh -c "chown postgres:postgres $3 && chmod $4 $3"; }
start() { # name extra-args...
  local n=$1; shift
  docker run -d --rm --name "$n" --network "$NET" -e POSTGRES_PASSWORD=test-only-not-secret -e PGDATA=/var/lib/postgresql/data \
    "$IMAGE" postgres -c shared_preload_libraries=pgbx -c pgbx.s3_endpoint="http://$S3:9000" -c pgbx.s3_bucket="$BUCKET" \
    -c pgbx.s3_region=us-east-1 -c pgbx.server_name="$SERVER" -c pgbx.credentials_file=/etc/pgbx/s3.credentials \
    -c pgbx.poll_seconds=2 "$@" >/dev/null
  docker exec -u root "$n" mkdir -p /etc/pgbx; put "$n" "$WORK/s3.credentials" /etc/pgbx/s3.credentials 600
  for _ in $(seq 120); do docker logs "$n" 2>&1 | grep -q "init process complete" && break; sleep 1; done
  local ok=0; for _ in $(seq 120); do if P "$n" -c "SELECT 1" >/dev/null 2>&1; then ok=$((ok+1)); [ $ok -ge 2 ] && break; else ok=0; fi; sleep 1; done; }
wait_job() { # container db id
  for _ in $(seq 120); do s=$(P "$1" -d "$2" -c "SELECT state FROM pgbx.history WHERE id=$3"); case "$s" in done|failed|cancelled) echo "$s"; return;; esac; sleep 1; done; echo timeout; }
wait_ext() { for _ in $(seq 60); do [ "$(P "$1" -d "$2" -c "SELECT count(*) FROM pg_extension WHERE extname='pgbx'" 2>/dev/null)" = 1 ] && return; sleep 1; done; }

echo "## server 1: notifications to a webhook; encryption turned on after a first plain backup"
start "$PG1"
put "$PG1" "$WORK/backup.key" /etc/pgbx/backup.key 600
put "$PG1" "$WORK/notify.secrets" /etc/pgbx/notify.secrets 600
# settings by ALTER SYSTEM (a -c on the command line would outrank every later ALTER SYSTEM)
P "$PG1" -c "ALTER SYSTEM SET pgbx.notify = 'webhook:hook'" -c "ALTER SYSTEM SET pgbx.notify_secrets_file = '/etc/pgbx/notify.secrets'" \
  -c "SELECT pg_reload_conf()" >/dev/null; sleep 2
P "$PG1" -c "CREATE ROLE app_owner LOGIN; CREATE ROLE app_ro NOLOGIN; CREATE ROLE other_tenant LOGIN; CREATE ROLE app_group NOLOGIN; GRANT app_group TO app_ro;"
P "$PG1" -c "CREATE DATABASE shop OWNER app_owner"
wait_ext "$PG1" shop
P "$PG1" -d shop -c "CREATE TABLE orders(id int primary key, item text); ALTER TABLE orders OWNER TO app_owner;
  GRANT SELECT ON orders TO app_ro; INSERT INTO orders SELECT g, 'item-'||g FROM generate_series(1,20000) g"
id0=$(P "$PG1" -d shop -c "SELECT pgbx.backup_now()" 2>/dev/null)
check "plain backup (encryption off) done" "$(wait_job "$PG1" shop "$id0")" done
check "history says not encrypted" "$(P "$PG1" -d shop -c "SELECT params->>'encrypted' FROM pgbx.history WHERE id=$id0")" false
plain_at=$(P "$PG1" -d shop -c "SELECT to_char(finished AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS')||'+00' FROM pgbx.history WHERE id=$id0")
sleep 1
P "$PG1" -c "ALTER SYSTEM SET pgbx.encryption_key_file = '/etc/pgbx/backup.key'" -c "SELECT pg_reload_conf()" >/dev/null; sleep 3
P "$PG1" -d shop -c "INSERT INTO orders VALUES (20001, 'after encryption')" >/dev/null
id=$(P "$PG1" -d shop -c "SELECT pgbx.backup_now()" 2>/dev/null)
check "encrypted backup done" "$(wait_job "$PG1" shop "$id")" done
check "history says encrypted" "$(P "$PG1" -d shop -c "SELECT params->>'encrypted' FROM pgbx.history WHERE id=$id")" true
key=$(P "$PG1" -d shop -c "SELECT s3_key FROM pgbx.history WHERE id=$id")
gkey=$(P "$PG1" -d shop -c "SELECT params->>'globals' FROM pgbx.history WHERE id=$id")
check "roles file key next to the dump" "$gkey" "${key%.dump}.globals.sql.zst"
roles=$(P "$PG1" -d shop -c "SELECT params->>'roles' FROM pgbx.history WHERE id=$id")
has "referenced roles recorded" "$roles" '"app_owner"'
has "parent role of a grantee referenced" "$roles" '"app_group"'
case "$roles" in *other_tenant*) check "unrelated role not referenced" yes no;; *) check "unrelated role not referenced" no no;; esac
AWS s3 cp "s3://$BUCKET/$key" /w/enc.bin >/dev/null 2>&1
check "the S3 object is ciphertext (PGBXENC1 header)" "$(head -c 8 "$WORK/enc.bin")" PGBXENC1
list=$(docker exec -u postgres "$PG1" pgbx backups --from-s3 --db shop --s3-endpoint "http://$S3:9000" --s3-bucket "$BUCKET" \
  --server-name "$SERVER" --credentials-file /etc/pgbx/s3.credentials --json)
check "backups --from-s3 lists dumps only, never a roles file" \
  "$(echo "$list" | grep -o '"key":"[^"]*"' | grep -vc '\.dump"')|$([ "$(echo "$list" | grep -o '"key":"[^"]*\.dump"' | wc -l)" -ge 2 ] && echo dumps)" "0|dumps"
id=$(P "$PG1" -d shop -c "SELECT pgbx.verify_now()" 2>/dev/null)
check "restore test of an encrypted dump" "$(wait_job "$PG1" shop "$id")" done
id=$(P "$PG1" -d shop -c "SELECT pgbx.restore('shop_plain', '$plain_at')" 2>/dev/null)
check "the older unencrypted dump still restores (key set)" "$(wait_job "$PG1" shop "$id")" done
check "  ... with its rows (as of the plain dump)" "$(P "$PG1" -d shop_plain -c "SELECT count(*) FROM orders")" 20000

echo "## restore with_roles on the same server: roles exist, owners kept"
id=$(P "$PG1" -d shop -c "SELECT pgbx.restore('shop_r', now(), with_roles => true)" 2>/dev/null)
check "restore with_roles" "$(wait_job "$PG1" shop "$id")" done
has "existing roles reported, not changed" "$(P "$PG1" -d shop -c "SELECT params->'roles'->>'existing' FROM pgbx.history WHERE id=$id")" app_owner
check "owner kept" "$(P "$PG1" -d shop_r -c "SELECT tableowner FROM pg_tables WHERE tablename='orders'")" app_owner
check "rows restored (decrypted)" "$(P "$PG1" -d shop_r -c "SELECT count(*) FROM orders")" 20001
check "restore without roles: owner is the restoring role" "$(P "$PG1" -d shop_plain -c "SELECT tableowner FROM pg_tables WHERE tablename='orders'")" postgres
has "bad roles scope refused" "$(P "$PG1" -d shop -c "SELECT pgbx.restore('x', now(), true, 'everyone')" 2>&1)" "referenced or all"

echo "## failure -> one webhook message, repeat suppressed, recovery once"
id=$(P "$PG1" -d shop -c "SELECT pgbx.restore('shop_r')" 2>/dev/null)   # exists: fails
check "restore into an existing database fails" "$(wait_job "$PG1" shop "$id")" failed
id=$(P "$PG1" -d shop -c "SELECT pgbx.restore('shop_r')" 2>/dev/null)
wait_job "$PG1" shop "$id" >/dev/null
id=$(P "$PG1" -d shop -c "SELECT pgbx.restore('shop_r2')" 2>/dev/null)
check "next restore ok" "$(wait_job "$PG1" shop "$id")" done
sleep 2
check "webhook: one failure message (deduplicated)" "$(grep -c '"event":"failure"' "$WORK/hook.log" 2>/dev/null)" 1
check "webhook: one recovered message" "$(grep -c '"event":"recovered"' "$WORK/hook.log" 2>/dev/null)" 1
case "$(cat "$WORK/hook.log" 2>/dev/null)" in *"$HOOK:8080"*) check "webhook body has no URL" leaked clean;; *) check "webhook body has no URL" clean clean;; esac
P "$PG1" -c "ALTER SYSTEM SET pgbx.notify = 'slack:https://hooks.example/x'" -c "SELECT pg_reload_conf()" >/dev/null; sleep 3
id=$(P "$PG1" -d shop -c "SELECT pgbx.restore('shop_r')" 2>/dev/null); wait_job "$PG1" shop "$id" >/dev/null
for _ in $(seq 10); do docker logs "$PG1" 2>&1 | grep -q 'not a URL' && break; sleep 1; done
check "a URL in pgbx.notify is refused (names only, logged)" "$(docker logs "$PG1" 2>&1 | grep -c 'not a URL')" 1
check "  ... and the log line does not quote the URL's host" "$(docker logs "$PG1" 2>&1 | grep 'not a URL' | grep -c 'hooks.example')" 0
P "$PG1" -c "ALTER SYSTEM SET pgbx.notify = 'webhook:hook'" -c "SELECT pg_reload_conf()" >/dev/null

echo "## GFS retention"
has "set_retention gfs" "$(P "$PG1" -d shop -c "SELECT pgbx.set_retention(gfs => '7d,4w,2m')")" "gfs 7d,4w,2m"
has "status shows gfs" "$(P "$PG1" -d shop -c "SELECT retention FROM pgbx.status()")" "gfs 7d,4w,2m"
has "gfs beyond max_days_limit refused" "$(P "$PG1" -d shop -c "SELECT pgbx.set_retention(gfs => '12m')" 2>&1)" "max_days_limit"
has "bad gfs refused" "$(P "$PG1" -d shop -c "SELECT pgbx.set_retention(gfs => '7x')" 2>&1)" "should look like"
r=$(docker exec -u postgres "$PG1" pgbx retention --db shop --gfs off --json)
has "pgbx retention: clearing GFS needs --yes" "$r" "long-term"
check "gfs off" "$(P "$PG1" -d shop -c "SELECT pgbx.set_retention(gfs => 'off')")" "keeping at most 14 backups and nothing older than 90 days (newest always kept); pruning now"
check "gfs cleared" "$(P "$PG1" -d shop -c "SELECT gfs IS NULL FROM pgbx.config")" t
r=$(docker exec -u postgres "$PG1" pgbx retention --db shop --gfs 7d,4w --json)
has "pgbx retention --gfs (adding: no --yes needed)" "$r" '"ok":true'
check "old 2-argument set_retention still works" "$(P "$PG1" -d shop -c "SELECT pgbx.set_retention(14, 90) LIKE 'keeping at most 14%gfs 7d,4w%'")" t

echo "## metrics"
sleep 4
m=$(docker exec -u postgres "$PG1" pgbx metrics)
has "metrics: encrypted flag" "$m" 'pgbx_last_backup_encrypted{database="shop"} 1'
has "metrics: restore test" "$m" 'pgbx_restore_test_ok{database="shop"} 1'
has "metrics: size" "$m" 'pgbx_last_backup_size_bytes{database="shop"}'
has "metrics: failures" "$m" 'pgbx_failed_jobs{database="shop"} 3'
has "metrics: up" "$m" 'pgbx_up 1'
docker exec -u postgres -d "$PG1" pgbx ui --listen 127.0.0.1:8432 >/dev/null; sleep 1
ui=$(docker exec -u postgres "$PG1" bash -c 'exec 3<>/dev/tcp/127.0.0.1/8432; printf "GET /metrics HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n" >&3; cat <&3')
has "ui /metrics content type" "$ui" "text/plain; version=0.0.4"
has "ui /metrics body" "$ui" 'pgbx_up 1'

echo "## server 2: a NEW server, db-restore --from-s3 --with-roles --key-file"
start "$PG2"
wait_ext "$PG2" postgres   # its own pgbx roles exist first, as on any server running pgbx
put "$PG2" "$WORK/backup.key" /tmp/backup.key 600
put "$PG2" "$WORK/wrong.key" /tmp/wrong.key 600
S3F="--s3-endpoint http://$S3:9000 --s3-bucket $BUCKET --s3-region us-east-1 --server-name $SERVER --credentials-file /etc/pgbx/s3.credentials"
R() { docker exec -u postgres "$PG2" bash -c "pgbx db-restore --from-s3 --db shop $S3F --json $*"; }
has "no key: refused" "$(R --into nokey)" "encrypted"
has "wrong key: refused" "$(R --into wrongkey --key-file /tmp/wrong.key)" "wrong key"
has "key file readable by others: refused" "$(docker exec -u root "$PG2" chmod 644 /tmp/backup.key; R --into k644 --key-file /tmp/backup.key; docker exec -u root "$PG2" chmod 600 /tmp/backup.key)" "chmod 600"
out=$(R --into shop --key-file /tmp/backup.key --with-roles)
has "restored with roles" "$out" '"ok":true'
has "roles created" "$out" '"created":["app_group","app_owner","app_ro"]'
has "other tenant's role not created" "$out" '"out_of_scope":["other_tenant"]'
check "owner on the new server" "$(P "$PG2" -d shop -c "SELECT tableowner FROM pg_tables WHERE tablename='orders'")" app_owner
check "grant on the new server" "$(P "$PG2" -d shop -c "SELECT has_table_privilege('app_ro', 'orders', 'SELECT')")" t
check "membership on the new server" "$(P "$PG2" -c "SELECT pg_has_role('app_ro', 'app_group', 'MEMBER')")" t
check "no passwords restored (backup_role_passwords off)" "$(P "$PG2" -c "SELECT count(*) FROM pg_authid WHERE rolname='app_owner' AND rolpassword IS NOT NULL")" 0
out=$(R --into shop2 --key-file /tmp/backup.key --with-roles)
has "second restore: roles already exist (idempotent)" "$out" '"created":[]'
has "  ... and are reported as existing" "$out" '"existing":["app_group","app_owner","app_ro",'
out=$(R --into shopold --time "'$plain_at'")
has "unencrypted dump restores with no key" "$out" '"encrypted":false'

echo "## pgbx decrypt (what a download link gives you is ciphertext)"
docker cp "$WORK/enc.bin" "$PG2:/tmp/enc.bin" >/dev/null
check "decrypt | pg_restore -l" "$(docker exec -u postgres "$PG2" bash -c "pgbx decrypt --key-file /tmp/backup.key --in /tmp/enc.bin | pg_restore -l | grep -c 'TABLE DATA public orders'")" 1
has "decrypt with the wrong key fails" "$(docker exec -u postgres "$PG2" bash -c "pgbx decrypt --key-file /tmp/wrong.key --in /tmp/enc.bin --out /tmp/x 2>&1")" "wrong key"
docker exec -u postgres "$PG2" sh -c 'head -c $(( $(wc -c < /tmp/enc.bin) - 100 )) /tmp/enc.bin > /tmp/cut.bin'
has "decrypt of a truncated file fails" "$(docker exec -u postgres "$PG2" bash -c "pgbx decrypt --key-file /tmp/backup.key --in /tmp/cut.bin --out /tmp/x 2>&1")" "truncated"
docker exec -u postgres "$PG2" sh -c "cp /tmp/enc.bin /tmp/flip.bin && printf 'X' | dd of=/tmp/flip.bin bs=1 seek=5000 conv=notrunc 2>/dev/null"
has "decrypt of a tampered file fails" "$(docker exec -u postgres "$PG2" bash -c "pgbx decrypt --key-file /tmp/backup.key --in /tmp/flip.bin --out /tmp/x 2>&1")" "modified"

echo "== extras_e2e: $pass passed, $fail failed"
[ $fail -eq 0 ]
