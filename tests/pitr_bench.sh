#!/usr/bin/env bash
# Point-in-time restore performance, throwaway containers (pgbx-pitr-bench-*), a local S3 (RustFS), PG ${PG_MAJOR:-16}.
# Measures, and compares with pgBackRest installed in the same container for the benchmark only (it is never part of
# pgbx; it talks to the same S3 through a TLS proxy because it speaks only https to S3):
#   a. wal-push latency per 16 MB segment, sync, p50/p99 over N segments          (pgBackRest archive-push, sync)
#   b. archiving throughput: pgbench (-i, then a timed run) writes with archiving blocked, then archive_command is
#      switched on and the backlog drains through async wal-push (parallel look-ahead) -> segments/s
#                                                                                   (pgBackRest archive-push async)
#   c. wal-get replay rate: N sequential restore_command calls, prefetch off vs on   (pgBackRest archive-get async)
#   d. base backup + point-in-time restore of a ~2 GB database: backup, restore (download + verify + unpack), then
#      recovery replays WAL to the latest moment and promotes; rows checked        (pgBackRest full backup / restore)
# Prints numbers; misses are reported, not hidden. N=${N:-200} segments, SIZE_ROWS=${SIZE_ROWS:-14000000},
# PGBENCH_SCALE=${PGBENCH_SCALE:-150} (pgbench -i: ~2.5 GB of WAL), PGBENCH_SECS=${PGBENCH_SECS:-30}. Needs ~12 GB of
# free Docker disk. IMAGE (pgbx:test).
set -u
PG=${PG_MAJOR:-16}; IMAGE=${PITR_IMAGE:-pgbx:test}; N=${N:-200}; ROWS=${SIZE_ROWS:-14000000}; PBS=${PGBENCH_SECS:-30}
SCALE=${PGBENCH_SCALE:-150}
NET=pgbx-pitr-bench-net; S3=pgbx-pitr-bench-s3; DB=pgbx-pitr-bench-db; TLS=pgbx-pitr-bench-tls; BIN=/usr/lib/postgresql/$PG/bin
DATA=/var/lib/postgresql/data; CONF=/var/lib/postgresql/pgbx/pgbx-wal.conf
P() { docker exec -u postgres "$DB" psql -v ON_ERROR_STOP=1 -qAt "$@"; }
X() { docker exec -u postgres "$DB" "$@"; }
XS() { docker exec -u postgres "$DB" bash -c "$1"; }
cleanup() { docker rm -fv "$DB" "$S3" "$TLS" >/dev/null 2>&1; docker network rm "$NET" >/dev/null 2>&1; [ -n "${TMPD:-}" ] && rm -rf "$TMPD"; }
trap '[ "${PITR_KEEP:-0}" = 1 ] || cleanup' EXIT
cleanup; TMPD=$(mktemp -d)
AK="bench$RANDOM$RANDOM"; SK=$(LC_ALL=C tr -dc 'a-z0-9' </dev/urandom | head -c 32)
printf 'access_key_id=%s\nsecret_access_key=%s\n' "$AK" "$SK" > "$TMPD/s3.credentials"
docker network create "$NET" >/dev/null
docker run -d --rm --name "$S3" --network "$NET" -e RUSTFS_ACCESS_KEY="$AK" -e RUSTFS_SECRET_KEY="$SK" rustfs/rustfs >/dev/null
for b in pitr pgbr; do for _ in $(seq 30); do
  docker run --rm --network "$NET" -e AWS_ACCESS_KEY_ID="$AK" -e AWS_SECRET_ACCESS_KEY="$SK" -e AWS_DEFAULT_REGION=us-east-1 \
    amazon/aws-cli --endpoint-url "http://$S3:9000" s3 mb "s3://$b" 2>&1 | grep -qE 'make_bucket|BucketAlready' && break; sleep 1; done; done
docker run -d --rm --name "$DB" --network "$NET" -e POSTGRES_PASSWORD=test-only-not-secret -e PGDATA=$DATA \
  -v "$TMPD/s3.credentials:/etc/pgbx/s3.credentials.src:ro" "$IMAGE" \
  sh -c 'mkdir -p /etc/pgbx && cp /etc/pgbx/s3.credentials.src /etc/pgbx/s3.credentials && chown postgres /etc/pgbx/s3.credentials && chmod 600 /etc/pgbx/s3.credentials && exec docker-entrypoint.sh "$@"' -- \
  postgres -c shared_preload_libraries=pgbx -c "pgbx.s3_endpoint=http://$S3:9000" -c pgbx.s3_bucket=pitr -c pgbx.server_name=bench \
  -c pgbx.credentials_file=/etc/pgbx/s3.credentials -c pgbx.poll_seconds=2 -c archive_mode=on \
  -c pgbx.pitr=on -c pgbx.pitr_schedule='0 0 1 1 *' -c max_wal_size=16GB -c pgbx.wal_queue_max=off >/dev/null
for _ in $(seq 120); do docker logs "$DB" 2>&1 | grep -q "init process complete" && break; sleep 1; done
for _ in $(seq 60); do P -c "SELECT 1" >/dev/null 2>&1 && break; sleep 1; done
# archive_command by ALTER SYSTEM, not -c: a command-line setting would outrank the switches in b.
P -c "ALTER SYSTEM SET archive_command = '/usr/local/bin/pgbx wal-push %p'" -c "SELECT pg_reload_conf()" >/dev/null
for _ in $(seq 60); do X test -f $CONF && break; sleep 1; done
# the first base backup (queued at enable) must not overlap the measurements
for _ in $(seq 300); do [ "$(P -c "SELECT count(*) FROM pgbx.history WHERE kind='base_backup' AND state IN ('queued','running')")" = 0 ] && break; sleep 1; done
echo "== pitr_bench PG $PG ($(docker exec "$DB" nproc) CPUs in the container), local S3 (RustFS), $(date -u +%Y-%m-%dT%H:%MZ)"
pct() { sort -n | awk -v p="$1" '{a[NR]=$1} END {i=int(NR*p/100); if (i<1) i=1; printf "%.1f", a[i]}'; }
ready() { P -c "SELECT count(*) FROM pg_ls_archive_statusdir() WHERE name LIKE '%.ready'"; }
now() { date +%s.%N; }
since() { awk -v s="$1" -v e="$(now)" 'BEGIN{printf "%.1f", e-s}'; }

echo "## setup: pgBackRest (benchmark only) against the same S3, through a TLS proxy"
docker run -d --rm --name $TLS --network "$NET" alpine sh -c "apk add -q socat openssl >/dev/null && \
  openssl req -x509 -newkey rsa:2048 -nodes -subj /CN=$TLS -keyout /k.pem -out /c.pem -days 2 2>/dev/null && \
  exec socat OPENSSL-LISTEN:443,reuseaddr,fork,cert=/c.pem,key=/k.pem,verify=0 TCP:$S3:9000" >/dev/null
docker exec "$DB" sh -c 'apt-get update -qq >/dev/null && apt-get install -y -qq pgbackrest >/dev/null 2>&1' && have_pgbr=1 || have_pgbr=0
PGBR="pgbackrest --config=/tmp/pgbr/pgbackrest.conf --stanza=main"
if [ $have_pgbr = 1 ]; then
  docker exec -e AK="$AK" -e SK="$SK" -e TLS="$TLS" -e DATA="$DATA" "$DB" sh -c 'mkdir -p /tmp/pgbr/spool /tmp/pgbr/log && cat > /tmp/pgbr/pgbackrest.conf <<C
[global]
repo1-type=s3
repo1-s3-endpoint=$TLS
repo1-s3-uri-style=path
repo1-s3-bucket=pgbr
repo1-s3-region=us-east-1
repo1-s3-key=$AK
repo1-s3-key-secret=$SK
repo1-storage-verify-tls=n
repo1-path=/bench
repo1-retention-full=2
compress-type=zst
compress-level=1
process-max=4
spool-path=/tmp/pgbr/spool
log-path=/tmp/pgbr/log
lock-path=/tmp/pgbr
start-fast=y
archive-check=n
[main]
pg1-path=$DATA
pg1-socket-path=/var/run/postgresql
C
chown -R postgres /tmp/pgbr; chmod 600 /tmp/pgbr/pgbackrest.conf'
  for _ in $(seq 30); do docker exec "$TLS" sh -c 'nc -z 127.0.0.1 443' 2>/dev/null && break; sleep 1; done
  X $PGBR stanza-create >"$TMPD/pgbr.out" 2>&1 || have_pgbr=0
  echo "  $(X pgbackrest version) stanza: $([ $have_pgbr = 1 ] && echo ok || echo "FAILED: $(tail -2 "$TMPD/pgbr.out" | tr '\n' ' ')")"
fi

echo "## a. wal-push latency (sync, one 16 MB segment per call, $N segments of real WAL)"
P -c "CREATE TABLE w (id bigserial, pad text)" >/dev/null
P -c "INSERT INTO w (pad) SELECT md5(g::text) FROM generate_series(1,200000) g" -c "SELECT pg_switch_wal()" >/dev/null
for _ in $(seq 60); do [ "$(ready)" = 0 ] && break; sleep 1; done
seg=$(P -c "SELECT last_archived_wal FROM pg_stat_archiver")
XS "rm -rf /tmp/b && mkdir -p /tmp/b/pg_wal/archive_status /tmp/b/pgbx /tmp/b/get && sed -e 's/^async=.*/async=off/' -e 's#^work_dir=.*#work_dir=/tmp/b/pgbx#' $CONF > /tmp/b/pgbx/c.conf"
XS "cd /tmp/b && pgbx wal-get $seg pg_wal/src --conf /tmp/b/pgbx/c.conf"
# copies of one real segment under fake names on timeline 0x77 (never collides with the server's own WAL)
XS "cd /tmp/b && for i in \$(seq 1 $N); do printf -v n '00000077000000%02X%08X' \$((i/256)) \$((i%256)); cp pg_wal/src pg_wal/\$n; done"
XS "cd /tmp/b && for f in pg_wal/00000077*; do s=\$(date +%s%N); pgbx wal-push \$f --conf /tmp/b/pgbx/c.conf || echo FAIL; e=\$(date +%s%N); echo \$(( (e-s)/1000000 )); done" > "$TMPD/push.ms"
echo "  pgbx wal-push: p50 $(pct 50 < "$TMPD/push.ms") ms, p99 $(pct 99 < "$TMPD/push.ms") ms, $(grep -c FAIL "$TMPD/push.ms") failures"
if [ $have_pgbr = 1 ]; then
  XS "cd /tmp/b && for f in pg_wal/00000077*; do s=\$(date +%s%N); $PGBR archive-push \$PWD/\$f >/dev/null 2>&1 || echo FAIL; e=\$(date +%s%N); echo \$(( (e-s)/1000000 )); done" > "$TMPD/pgbr.ms"
  echo "  pgBackRest archive-push: p50 $(pct 50 < "$TMPD/pgbr.ms") ms, p99 $(pct 99 < "$TMPD/pgbr.ms") ms, $(grep -c FAIL "$TMPD/pgbr.ms") failures"
fi

drain() { # label archive_command -> generate a backlog with pgbench, switch archiving on, time the drain
  P -c "ALTER SYSTEM SET archive_command = '/bin/false'" -c "SELECT pg_reload_conf()" >/dev/null; sleep 2
  local c0 t0 tps n c1 secs
  XS "pgbench -i -q -s $SCALE bench >/dev/null 2>&1"   # bulk load: most of the backlog
  tps=$(XS "pgbench -n -c 8 -j 4 -T $PBS bench 2>/dev/null | grep -o 'tps = [0-9.]*' | head -1")
  P -c "SELECT pg_switch_wal()" >/dev/null; sleep 1
  n=$(ready); c0=$(P -c "SELECT archived_count FROM pg_stat_archiver")
  t0=$(now)
  P -c "ALTER SYSTEM SET archive_command = '$2'" -c "SELECT pg_reload_conf()" >/dev/null
  for _ in $(seq 1800); do [ "$(ready)" = 0 ] && break; sleep 0.2; done
  secs=$(since "$t0"); c1=$(P -c "SELECT archived_count FROM pg_stat_archiver")
  echo "  $1: pgbench -i -s $SCALE + $PBS s of 8 clients ($tps) left a backlog of $n segments ($((n*16)) MB); drained $((c1-c0)) in $secs s =" \
       "$(awk -v n="$((c1-c0))" -v s="$secs" 'BEGIN{printf "%.1f segments/s (%.0f MB/s)", n/s, n*16/s}'); failed_count $(P -c "SELECT failed_count FROM pg_stat_archiver")"
}
echo "## b. archiving throughput: a pgbench backlog drained by the archiver (target >= 50 segments/s)"
P -c "CREATE DATABASE bench" >/dev/null
for _ in $(seq 30); do P -d bench -c "SELECT pgbx.pause('pitr bench: no per-database dumps during measurements')" >/dev/null 2>&1 && break; sleep 1; done
drain "pgbx wal-push (async, process_max 4)" "/usr/local/bin/pgbx wal-push %p"
[ $have_pgbr = 1 ] && drain "pgBackRest archive-push (async, process-max 4)" "$PGBR --archive-async archive-push %p"
P -c "ALTER SYSTEM SET archive_command = '/usr/local/bin/pgbx wal-push %p'" -c "SELECT pg_reload_conf()" >/dev/null
P -c "DROP DATABASE bench" >/dev/null   # disk: section d wants a ~2 GB server, not 2 GB more of pgbench tables

echo "## c. wal-get replay: $N sequential restore_command calls"
for pf in 0 16; do
  XS "sed -i 's/^prefetch=.*/prefetch=$pf/; s/^process_max=.*/process_max=8/' /tmp/b/pgbx/c.conf; grep -q '^prefetch=' /tmp/b/pgbx/c.conf || echo prefetch=$pf >> /tmp/b/pgbx/c.conf; rm -rf /tmp/b/pgbx/spool"
  s=$(now)
  XS "cd /tmp/b && for f in \$(ls pg_wal | grep ^00000077); do pgbx wal-get \$f get/x --conf /tmp/b/pgbx/c.conf || echo MISS; done" > "$TMPD/get$pf"
  e=$(since "$s")
  echo "  pgbx prefetch=$pf: $(awk -v s="$e" -v n="$N" 'BEGIN{printf "%.1f segments/s (%.0f MB/s of WAL)", n/s, n*16/s}'), misses $(grep -c MISS "$TMPD/get$pf")"
done
if [ $have_pgbr = 1 ]; then
  for mode in n y; do
    XS "rm -rf /tmp/pgbr/spool/archive; mkdir -p /tmp/b/get"
    s=$(now)
    XS "cd /tmp/b && for f in \$(ls pg_wal | grep ^00000077); do $PGBR --archive-async=$mode --archive-get-queue-max=268435456 archive-get \$f \$PWD/get/x >/dev/null 2>&1 || echo MISS; done" > "$TMPD/pgbrget$mode"
    e=$(since "$s")
    echo "  pgBackRest archive-get async=$mode: $(awk -v s="$e" -v n="$N" 'BEGIN{printf "%.1f segments/s (%.0f MB/s of WAL)", n/s, n*16/s}'), misses $(grep -c MISS "$TMPD/pgbrget$mode")"
  done
fi
XS "rm -rf /tmp/b/pg_wal/00000077*"

echo "## d. base backup + point-in-time restore of a ~2 GB database"
P -c "CREATE TABLE big AS SELECT g AS id, md5(g::text)||md5((g+1)::text) AS a, md5((g*3)::text) AS b FROM generate_series(1,$ROWS) g" >/dev/null
echo "  databases: $(P -c "SELECT pg_size_pretty(sum(pg_database_size(datname))::bigint) FROM pg_database")"
s=$(now); r=$(X pgbx pitr backup --conf $CONF --json)
echo "  pgbx base backup: $(since "$s") s wall; $(echo "$r" | grep -o '"mb_per_s":[0-9.]*') of tar; $(echo "$r" | grep -o '"tar_bytes":[0-9]*'), compressed $(echo "$r" | grep -o '"bytes":[0-9]*' | head -1); ok=$(echo "$r" | grep -c '"ok":true')"
P -c "INSERT INTO big SELECT g, 'after', 'base backup' FROM generate_series($ROWS + 1, $ROWS + 1000) g" -c "SELECT pg_switch_wal()" >/dev/null
for _ in $(seq 120); do [ "$(ready)" = 0 ] && break; sleep 1; done
want=$(P -c "SELECT count(*) FROM big")
s=$(now); r=$(X pgbx pitr restore --time latest --target /tmp/rb --conf $CONF --json)
echo "  pgbx pitr restore (download + sha256 + unpack): $(since "$s") s wall; $(echo "$r" | grep -o '"mb_per_s":[0-9.]*'); ok=$(echo "$r" | grep -c '"ok":true')"
s=$(now); X $BIN/pg_ctl -D /tmp/rb -o "-p 5433 -k /tmp" -l /tmp/rb/pgbx-restore/recovery.log -w -t 900 start >/dev/null
for _ in $(seq 900); do [ "$(docker exec -u postgres "$DB" psql -h /tmp -p 5433 -qAt -c "SELECT pg_is_in_recovery()" 2>/dev/null)" = f ] && break; sleep 1; done
echo "  recovery (WAL replay from S3 via wal-get + promote): $(since "$s") s; rows $(docker exec -u postgres "$DB" psql -h /tmp -p 5433 -qAt -c "SELECT count(*) FROM big") of $want"
X $BIN/pg_ctl -D /tmp/rb -m fast stop >/dev/null 2>&1; docker exec "$DB" rm -rf /tmp/rb
if [ $have_pgbr = 1 ]; then
  s=$(now); X $PGBR --type=full backup >"$TMPD/pgbrb.out" 2>&1 || tail -3 "$TMPD/pgbrb.out"
  echo "  pgBackRest full backup: $(since "$s") s"
  s=$(now); X $PGBR --pg1-path=/tmp/rp --type=none restore >"$TMPD/pgbrr.out" 2>&1 || tail -3 "$TMPD/pgbrr.out"
  echo "  pgBackRest restore (files only): $(since "$s") s"
  docker exec "$DB" rm -rf /tmp/rp
fi
echo "== pitr_bench done"
