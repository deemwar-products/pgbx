#!/usr/bin/env bash
# S3 credentials from the EC2 instance role (pgbx.credentials_file = 'aws-default'), end to end (bash 3.2-safe), in
# throwaway containers (never the compose stacks):
#   pgbx-imds-s3    MinIO with two users that take turns as "the role" (STS AssumeRole: real session tokens)
#   pgbx-imds-meta  a fake IMDSv2 endpoint (tests/fake_imds.py): token PUT, role list, role credentials. Every ROT
#                   seconds it rotates to the other user and GRACE seconds later DISABLES the previous one, so MinIO
#                   refuses every credential a client did not refresh in time
#   pgbx-imds-pg    Postgres + pgbx with no keys file, AWS_EC2_METADATA_SERVICE_ENDPOINT pointing at the fake
#   1 doctor reports the source (instance-role, the role name); backup_now and restore work
#   2 a multipart upload slowed to ~90 s by pgbx.upload_kbps spans two rotations: refreshed in time (no 403, no
#     retry), complete, and restores to the same data; the retired keys really are refused by then
#   3 the CLI: backups --from-s3, db-restore --from-s3 and pitr list with no --credentials-file (aws-default), env
#     keys when set; setup server --credentials aws-default plan
#   4 no key, secret or token (nor an IMDS session token) in the Postgres log, history, server_overview, doctor or
#     any CLI output
#   5 an IMDSv1-only endpoint (the token PUT is refused) is refused, with no IMDSv1 GET; no role attached is named
# IMAGE (pgbx:test, built by tests/run_all.sh). PGBX_IMDS_KEEP=1 leaves the containers running.
set -u
IMAGE=${IMAGE:-pgbx:test}
HERE=$(cd "$(dirname "$0")" && pwd)
NET=pgbx-imds-net; S3=pgbx-imds-s3; META=pgbx-imds-meta; PG=pgbx-imds-pg
BUCKET=imds; SERVER="imds-$(date -u +%Y%m%d%H%M%S)"; ROT=30; GRACE=10
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "  PASS $1"; pass=$((pass+1)); else echo "  FAIL $1 (got '$2', want '$3')"; fail=$((fail+1)); fi; }
has() { case "$2" in *"$3"*) echo "  PASS $1"; pass=$((pass+1));; *) echo "  FAIL $1 ('$3' not in: $(echo "$2" | head -c 600))"; fail=$((fail+1));; esac; }
P() { docker exec -u postgres "$PG" psql -v ON_ERROR_STOP=1 -qAt "$@"; }
cleanup() { docker rm -fv "$PG" "$META" "$S3" >/dev/null 2>&1; docker network rm "$NET" >/dev/null 2>&1; [ -n "${WORK:-}" ] && rm -rf "$WORK"; }
trap '[ "${PGBX_IMDS_KEEP:-0}" = 1 ] || cleanup' EXIT
cleanup
WORK=$(mktemp -d)
rnd() { LC_ALL=C tr -dc 'a-z0-9' </dev/urandom | head -c "$1"; }
RK="root$(rnd 8)"; RS=$(rnd 32); UA="rolea$(rnd 6)"; UAS=$(rnd 32); UB="roleb$(rnd 6)"; UBS=$(rnd 32)
printf '%s %s\n' "$RK" "$RS" > "$WORK/root"; printf '%s %s\n%s %s\n' "$UA" "$UAS" "$UB" "$UBS" > "$WORK/users"
cp "$HERE/fake_imds.py" "$WORK/"; chmod -R a+rwX "$WORK"
# MinIO stopped publishing images (minio/minio is gone from Docker Hub, quay.io needs a login, dl.min.io answers 410):
# Chainguard's free builds, the server and mc in separate images. Override with PGBX_MINIO_IMAGE / PGBX_MC_IMAGE.
MINIO_IMAGE=${PGBX_MINIO_IMAGE:-cgr.dev/chainguard/minio:latest}; MC_IMAGE=${PGBX_MC_IMAGE:-cgr.dev/chainguard/minio-client:latest}
mc_() { docker run --rm --network "$NET" -e MC_HOST_local="http://$RK:$RS@$S3:9000" "$MC_IMAGE" "$@"; }
MC() { mc_ "$@" >/dev/null 2>&1; }
wait_job() { # db id
  for _ in $(seq 240); do s=$(P -d "$1" -c "SELECT state FROM pgbx.history WHERE id=$2"); case "$s" in done|failed|cancelled) echo "$s"; return;; esac; sleep 1; done; echo timeout; }
doctor() { P -c "SELECT ok || ' | ' || detail || ' | ' || coalesce(fix, '') FROM pgbx.doctor() WHERE name = 's3 credentials'"; }
# the CLI as the postgres OS user inside the Postgres container (it inherits AWS_EC2_METADATA_SERVICE_ENDPOINT)
S3F="--s3-endpoint http://$S3:9000 --s3-bucket $BUCKET --s3-region us-east-1 --server-name $SERVER"
CLI() { docker exec -u postgres "$PG" pgbx "$@"; }
OUT="$WORK/outputs.txt"; : > "$OUT"   # every output a secret must never appear in

echo "== imds_e2e image $IMAGE (server folder $SERVER, rotation every ${ROT}s, old keys disabled ${GRACE}s later)"
docker network create "$NET" >/dev/null
docker run -d --rm --name "$S3" --network "$NET" -e MINIO_ROOT_USER="$RK" -e MINIO_ROOT_PASSWORD="$RS" "$MINIO_IMAGE" server /tmp/data >/dev/null
for _ in $(seq 60); do MC ls local && break; sleep 1; done
MC mb "local/$BUCKET"
for u in "$UA $UAS" "$UB $UBS"; do set -- $u; MC admin user add local "$1" "$2"; MC admin policy attach local readwrite --user "$1"; done
check "MinIO users for the role" "$(mc_ admin user list local 2>/dev/null | grep -c readwrite)" 2
docker run -d --rm --name "$META" --network "$NET" -v "$WORK:/w" -e S3="http://$S3:9000" -e ROT=$ROT -e GRACE=$GRACE \
  python:3.11-slim python3 /w/fake_imds.py >/dev/null
for _ in $(seq 60); do grep -q ready "$WORK/requests.log" 2>/dev/null && break; sleep 1; done
check "fake IMDS issued first credentials" "$(grep -c 'rotation 1 ' "$WORK/rotations.log" 2>/dev/null)" 1

docker run -d --rm --name "$PG" --network "$NET" -e POSTGRES_PASSWORD=test-only-not-secret -e PGDATA=/var/lib/postgresql/data \
  -e AWS_EC2_METADATA_SERVICE_ENDPOINT="http://$META" \
  "$IMAGE" postgres -c shared_preload_libraries=pgbx -c pgbx.s3_endpoint="http://$S3:9000" -c pgbx.s3_bucket="$BUCKET" \
  -c pgbx.s3_region=us-east-1 -c pgbx.server_name="$SERVER" -c pgbx.credentials_file=aws-default -c pgbx.poll_seconds=2 >/dev/null
for _ in $(seq 120); do docker logs "$PG" 2>&1 | grep -q "init process complete" && break; sleep 1; done
ok=0; for _ in $(seq 120); do if P -c "SELECT 1" >/dev/null 2>&1; then ok=$((ok+1)); [ $ok -ge 2 ] && break; else ok=0; fi; sleep 1; done

echo "## 1 doctor, backup and restore with the instance role"
# the worker creates the extension in the admin database (postgres) within a poll or two
for _ in $(seq 60); do [ "$(P -c "SELECT count(*) FROM pg_extension WHERE extname='pgbx'" 2>/dev/null)" = 1 ] && break; sleep 1; done
d=$(doctor); echo "$d" >> "$OUT"
has "doctor: s3 credentials ok" "$d" "true | source: instance-role"
has "doctor names the role and IMDSv2" "$d" "instance role via IMDSv2 (role pgbx-imds-role), temporary, valid until"
check "doctor: s3 settings ok without a credentials file" "$(P -c "SELECT ok FROM pgbx.doctor() WHERE name='s3 settings'")" t
P -c "CREATE DATABASE shop"
for _ in $(seq 60); do [ "$(P -d shop -c "SELECT count(*) FROM pg_extension WHERE extname='pgbx'" 2>/dev/null)" = 1 ] && break; sleep 1; done
P -d shop -c "CREATE TABLE orders(id int primary key, item text); INSERT INTO orders SELECT g, 'item-'||g FROM generate_series(1,5000) g"
id=$(P -d shop -c "SELECT pgbx.backup_now()" 2>/dev/null)
check "backup_now with aws-default" "$(wait_job shop "$id")" done
id=$(P -d shop -c "SELECT pgbx.restore('shop_copy')" 2>/dev/null)
check "restore with aws-default" "$(wait_job shop "$id")" done
check "restored rows" "$(P -d shop_copy -c "SELECT count(*) FROM orders")" 5000
has "worker logged where the credentials came from" "$(docker logs "$PG" 2>&1 | grep 's3 credentials from' | head -1)" \
  "pgbx: s3 credentials from instance role via IMDSv2 (role pgbx-imds-role), temporary, valid until"
check "every IMDS GET carried an IMDSv2 token" "$(grep '^GET' "$WORK/requests.log" | grep -c 'token=no')" 0

echo "## 2 a long multipart upload across two credential rotations"
P -d shop -c "CREATE EXTENSION IF NOT EXISTS pgcrypto; CREATE TABLE big AS SELECT g AS id, gen_random_bytes(1000) AS b FROM generate_series(1, 90000) g"
P -c "ALTER SYSTEM SET pgbx.upload_kbps = 1024" -c "SELECT pg_reload_conf()" >/dev/null; sleep 3
rot0=$(grep -c ' rotation ' "$WORK/rotations.log"); ref0=$(docker logs "$PG" 2>&1 | grep -c 's3 credentials from')
t0=$(date +%s)
id=$(P -d shop -c "SELECT pgbx.backup_now()" 2>/dev/null)
check "long upload done" "$(wait_job shop "$id")" done
secs=$(( $(date +%s) - t0 ))
rot1=$(grep -c ' rotation ' "$WORK/rotations.log"); ref1=$(docker logs "$PG" 2>&1 | grep -c 's3 credentials from')
bytes=$(P -d shop -c "SELECT bytes FROM pgbx.history WHERE id=$id")
echo "  (upload: ${secs}s, ${bytes} bytes; IMDS rotations $rot0 -> $rot1; worker refreshes $ref0 -> $ref1)"
check "upload took longer than two rotations" "$([ "$secs" -ge $((2*ROT)) ] && echo yes)" yes
check "multipart (more than 3 parts of 16 MiB)" "$([ "${bytes:-0}" -gt $((48*1024*1024)) ] && echo yes)" yes
check "credentials rotated at least twice during it" "$([ $((rot1-rot0)) -ge 2 ] && echo yes)" yes
check "the worker refreshed during it" "$([ $((ref1-ref0)) -ge 2 ] && echo yes)" yes
check "no retry and no 403 on the way (refreshed BEFORE expiry)" \
  "$(docker logs "$PG" 2>&1 | grep -E 'retry [0-9]|ExpiredToken|InvalidAccessKeyId|HTTP 403' | grep -vc 'pg_dump --version')" 0
# keys from the newest rotation of the user that is NOT the role now (disabled GRACE s after it was retired) are
# refused: the refresh was necessary, not cosmetic. issued.txt holds 3 lines per rotation, in rotation order.
sleep $((GRACE + 2))
cur=$(grep ' rotation ' "$WORK/rotations.log" | tail -1 | awk '{print $NF}')
n=$(grep ' rotation ' "$WORK/rotations.log" | awk -v c="$cur" '$NF != c {n=$3} END {print n}')
first_ak=$(sed -n "$((3*n-2))p" "$WORK/issued.txt"); first_sk=$(sed -n "$((3*n-1))p" "$WORK/issued.txt"); first_tok=$(sed -n "$((3*n))p" "$WORK/issued.txt")
old=$(docker run --rm --network "$NET" -e AWS_ACCESS_KEY_ID="$first_ak" -e AWS_SECRET_ACCESS_KEY="$first_sk" \
  -e AWS_SESSION_TOKEN="$first_tok" -e AWS_DEFAULT_REGION=us-east-1 amazon/aws-cli --endpoint-url "http://$S3:9000" \
  s3 ls "s3://$BUCKET/" 2>&1 | grep -oE 'AccessDenied|InvalidAccessKeyId|ExpiredToken|InvalidToken|SignatureDoesNotMatch' | head -1)
check "keys of a retired rotation are refused by S3" "$([ -n "$old" ] && echo refused)" refused
P -c "ALTER SYSTEM RESET pgbx.upload_kbps" -c "SELECT pg_reload_conf()" >/dev/null; sleep 2
id=$(P -d shop -c "SELECT pgbx.restore('shop_big')" 2>/dev/null)
check "restore of the long upload" "$(wait_job shop "$id")" done
check "same data after the round trip" "$(P -d shop_big -c "SELECT count(*), md5(string_agg(md5(b), '' ORDER BY id)) FROM big")" \
  "$(P -d shop -c "SELECT count(*), md5(string_agg(md5(b), '' ORDER BY id)) FROM big")"

echo "## 3 the CLI with aws-default"
l=$(CLI backups --from-s3 --db shop $S3F --json 2>&1); echo "$l" >> "$OUT"
check "backups --from-s3, no --credentials-file (instance role)" "$(echo "$l" | grep -o '"count":[0-9]*' | awk -F: '$2 >= 2 {print "2+"}')" "2+"
r=$(CLI db-restore --from-s3 --db shop --into shop_cli $S3F --credentials-file aws-default --json 2>&1); echo "$r" >> "$OUT"
has "db-restore --from-s3 --credentials-file aws-default" "$r" '"ok":true'
check "rows restored by the CLI" "$(P -d shop_cli -c "SELECT count(*) FROM orders")" 5000
p=$(CLI pitr list $S3F --system-id 1 --json 2>&1); echo "$p" >> "$OUT"
has "pitr list (S3 engine of wal-push / wal-get) with the instance role" "$p" '"ok":true'
e=$(docker exec -u postgres -e AWS_ACCESS_KEY_ID=nobody -e AWS_SECRET_ACCESS_KEY=wrong "$PG" pgbx backups --from-s3 --db shop $S3F --json 2>&1)
echo "$e" >> "$OUT"
has "CLI: env keys come before the instance role (these are wrong, so S3 refuses)" "$e" '"ok":false'
s=$(docker exec -u root "$PG" pgbx setup server --credentials aws-default $S3F --user postgres --json 2>&1); echo "$s" >> "$OUT"
has "setup server --credentials aws-default: plan sets the setting" "$s" '"pgbx.credentials_file":"aws-default"'
has "setup server --credentials aws-default: no keys file" "$s" 'no keys file: S3 credentials from the instance role'

echo "## 4 no key, secret or token anywhere"
cat "$WORK/issued.txt" "$WORK/imds_tokens.txt" | grep -v '^$' > "$WORK/secrets.txt"
check "secrets to look for" "$([ "$(wc -l < "$WORK/secrets.txt")" -ge 9 ] && echo yes)" yes
docker logs "$PG" > "$WORK/pg.log" 2>&1
check "none in the Postgres log" "$(grep -c -F -f "$WORK/secrets.txt" "$WORK/pg.log")" 0
for db in shop postgres; do
  P -d "$db" -c "SELECT h::text FROM pgbx.history h" > "$WORK/hist.txt" 2>/dev/null
  check "none in $db's history" "$(grep -c -F -f "$WORK/secrets.txt" "$WORK/hist.txt")" 0
done
P -c "SELECT o::text FROM pgbx.server_overview o" > "$WORK/ov.txt"; P -c "SELECT d::text FROM pgbx.doctor() d" >> "$WORK/ov.txt"
check "none in server_overview or doctor()" "$(grep -c -F -f "$WORK/secrets.txt" "$WORK/ov.txt")" 0
check "none in CLI output" "$(grep -c -F -f "$WORK/secrets.txt" "$OUT")" 0

echo "## 5 IMDSv1 only, and no role attached"
# the fake's mode and request log, read inside its container (a bind mount on Docker Desktop may lag)
mode() { docker exec "$META" python3 -c 'import sys, urllib.request as u; u.urlopen(u.Request("http://127.0.0.1/_mode/" + sys.argv[1], method="PUT"))' "$1"; }
gets() { docker exec "$META" grep -c '^GET' /w/requests.log; }
mode v1only; n0=$(gets)
d=$(doctor)   # a new session: its own cache, asks the endpoint now
has "doctor: IMDSv1-only endpoint refused" "$d" "false | s3 credentials: none found for pgbx.credentials_file = 'aws-default'"
has "doctor says why: no IMDSv2 token, no fall-back" "$d" "no IMDSv2 session token (HTTP 405); pgbx never falls back to IMDSv1"
has "doctor fix: the instance profile policy" "$d" "attach an instance profile whose role allows s3:PutObject, s3:GetObject, s3:ListBucket"
e=$(CLI backups --from-s3 --db shop $S3F --json 2>&1)
has "CLI refuses IMDSv1 too" "$e" "never falls back to IMDSv1"
check "no IMDSv1 GET was made" "$(gets)" "$n0"
mode norole
has "no role attached is named" "$(CLI backups --from-s3 --db shop $S3F --json 2>&1)" "no IAM role is attached to this instance"
mode v2
has "back to IMDSv2: doctor ok again" "$(doctor)" "true | source: instance-role"

echo "== imds_e2e: $pass passed, $fail failed"
[ $fail -eq 0 ]
