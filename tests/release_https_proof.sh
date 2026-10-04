#!/usr/bin/env bash
# Real HTTPS S3 backup + restore with a RELEASED pgbx, installed by the public install.sh into postgres:16.
# Proves what the plain-http suites cannot: a stored backup on a real https S3 endpoint, restored with the same data.
# Credentials come from `sec` (S3_ENDPOINT without scheme, S3_REGION, BACKUP_S3_BUCKET, S3_ACCESS_KEY, S3_SECRET_KEY);
# their values never reach the output. Usage: tests/release_https_proof.sh v0.6.1   (0.6.0 panics: CryptoProvider)
set -u
V=$1; D=$(cd "$(dirname "$0")" && pwd)/release_proof; C=pgbx-proof-${V//./}; SRV=release-proof-$V-$(date -u +%Y%m%dT%H%M%S)
docker build -q --build-arg PGBX_VERSION=$V -t pgbx-proof:$V "$D" >/dev/null || { echo "build failed"; exit 1; }
docker rm -f -v $C >/dev/null 2>&1
sec exec -- docker run -d --name $C -e POSTGRES_PASSWORD=proof-only -e PGBX_SERVER=$SRV \
  -e S3_ENDPOINT=https://{{S3_ENDPOINT}} -e S3_REGION={{S3_REGION}} -e BACKUP_S3_BUCKET={{BACKUP_S3_BUCKET}} \
  -e S3_ACCESS_KEY={{S3_ACCESS_KEY}} -e S3_SECRET_KEY={{S3_SECRET_KEY}} pgbx-proof:$V >/dev/null || { echo "run failed"; exit 1; }
P() { docker exec $C psql -U postgres -qAtX -v ON_ERROR_STOP=1 "$@"; }
for _ in $(seq 60); do P -c 'SELECT 1' >/dev/null 2>&1 && break; sleep 1; done
P -c 'CREATE DATABASE shop'
P -d shop -c "CREATE TABLE orders AS SELECT g AS id, md5(g::text) AS v FROM generate_series(1,200000) g"
for _ in $(seq 60); do P -d shop -c "SELECT 1 FROM pg_extension WHERE extname='pgbx'" | grep -q 1 && break; sleep 1; done
echo "date: $(date -u +%FT%TZ)  release: $V  cli: $(docker exec $C pgbx --version)"
echo "version: $(P -d shop -c "SELECT extversion FROM pg_extension WHERE extname='pgbx'")  endpoint scheme: $(docker exec $C sh -c "echo \$S3_ENDPOINT | cut -d: -f1")  server folder: $SRV"
wait_job() { for _ in $(seq 300); do s=$(P -d shop -c "SELECT state||'|'||coalesce(bytes,0)||'|'||coalesce(error,'') FROM pgbx.history WHERE id=$1"); case "$s" in done*|failed*|cancelled*) echo "$s"; return;; esac; sleep 2; done; echo "timeout|$s"; }
b=$(P -d shop -c 'SELECT pgbx.backup_now()' 2>/dev/null | tail -1); echo "backup job $b: $(wait_job $b)"
r=$(P -d shop -c "SELECT pgbx.restore(into_db => 'shop_restored')" 2>/dev/null | tail -1); echo "restore job $r: $(wait_job $r)"
src=$(P -d shop -c "SELECT count(*)||' '||md5(string_agg(v,'' ORDER BY id)) FROM orders")
dst=$(P -d shop_restored -c "SELECT count(*)||' '||md5(string_agg(v,'' ORDER BY id)) FROM orders" 2>/dev/null)
echo "source:   $src"; echo "restored: ${dst:-<none>}"
docker logs $C 2>&1 | grep -m3 -i 'panic\|CryptoProvider' | cut -c1-200
[ -n "$dst" ] && [ "$src" = "$dst" ] && echo "PROOF PASS" || echo "PROOF FAIL"
docker rm -f -v $C >/dev/null 2>&1
docker image rm pgbx-proof:$V >/dev/null 2>&1
