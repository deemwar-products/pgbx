#!/usr/bin/env bash
# What a backup costs the application (ADR 0001 §3, M8): pgbench TPS and latency (avg, p95) three ways:
#   none      no backup running
#   uncapped  backups back to back, pg_dump at the worker's own priority (pgbx.job_nice = 0, pgbx.job_ionice = none)
#   capped    backups back to back with the defaults (nice 10, IO best-effort 7)
# Budget (ADR 0001): with caps, p95 latency +<= 15 % and TPS -<= 10 % against "none".
# Runs on docker/compose.local.yml (Postgres + pgbx + a local S3 on one Docker network). Manual, never in CI.
#   SCALE=50 CLIENTS=16 SECS=300 bench/load_overhead.sh       (ADR defaults; SECS=120 for a quicker look)
# Prints a markdown table to paste into bench/RESULTS.md. Needs pgbx:test (docker compose -f compose.test.yml build).
set -u
cd "$(dirname "$0")/../docker"
DC="docker compose -f compose.local.yml"
SCALE=${SCALE:-50}; CLIENTS=${CLIENTS:-16}; SECS=${SECS:-300}; THREADS=${THREADS:-4}
P() { $DC exec -T db psql -U postgres -v ON_ERROR_STOP=1 -qAt "$@"; }
X() { $DC exec -T -u postgres db "$@"; }
gset() { P -c "ALTER SYSTEM SET pgbx.$1 = '$2'" -c "SELECT pg_reload_conf()" >/dev/null; }

$DC up -d >/dev/null 2>&1
ok=0; for _ in $(seq 120); do if P -c "SELECT 1" >/dev/null 2>&1; then ok=$((ok+1)); [ $ok -ge 3 ] && break; else ok=0; fi; sleep 1; done
P -c "DROP DATABASE IF EXISTS bench WITH (FORCE)" -c "CREATE DATABASE bench" >/dev/null
for _ in $(seq 60); do P -d bench -c "SELECT 1 FROM pgbx.config" >/dev/null 2>&1 && break; sleep 1; done
P -d bench -c "SELECT pgbx.pause('bench: only the backups this script asks for')" >/dev/null
t=$SECONDS; X pgbench -i -q -s "$SCALE" bench >/dev/null 2>&1
echo "# pgbench scale $SCALE ($(P -c "SELECT pg_size_pretty(pg_database_size('bench'))")) loaded in $((SECONDS-t))s; $CLIENTS clients, $THREADS threads, ${SECS}s per run" >&2
cores=$(X nproc 2>/dev/null || echo '?')

# one pgbench run; meanwhile (unless mode=none) back-to-back backups of the same database. Prints: tps avg_ms p95_ms backups
run() {
  local mode=$1 stop=/tmp/bench.stop n=0
  X rm -f /tmp/pgb.* "$stop"
  if [ "$mode" != none ]; then
    ( while ! X test -f "$stop"; do
        id=$(P -d bench -c "SELECT pgbx.backup_now()" 2>/dev/null)
        for _ in $(seq 3600); do s=$(P -d bench -c "SELECT state FROM pgbx.history WHERE id=$id"); case "$s" in done|failed|cancelled) break;; esac; X test -f "$stop" && break; sleep 1; done
      done ) & loop=$!
    sleep 3   # the first dump is under way before pgbench starts
  fi
  out=$(X pgbench -c "$CLIENTS" -j "$THREADS" -T "$SECS" -l --log-prefix=/tmp/pgb bench 2>&1)
  X touch "$stop"
  [ "$mode" != none ] && wait $loop 2>/dev/null
  n=$(P -d bench -c "SELECT count(*) FROM pgbx.history WHERE kind='backup' AND state='done' AND finished > now() - make_interval(secs => $SECS + 60)")
  tps=$(echo "$out" | sed -n 's/^tps = \([0-9.]*\).*/\1/p' | tail -1 | awk '{printf "%.0f", $1}')
  lat=$(X sh -c 'cat /tmp/pgb.* | awk "{print \$3}" | sort -n | awk "{a[NR]=\$1; s+=\$1} END {printf \"%.2f %.2f\", s/NR/1000, a[int(NR*0.95)]/1000}"')
  echo "$tps $lat $n"
  P -d bench -c "SELECT pg_sleep(5)" >/dev/null   # let the last dump finish and the server settle
}

echo "| run | backups during run | TPS | vs none | avg latency ms | p95 ms | vs none |"
echo "|---|---|---|---|---|---|---|"
gset job_nice 10; gset job_ionice best-effort-7
read -r tps0 avg0 p950 _ <<<"$(run none)"
echo "| none | 0 | $tps0 | | $avg0 | $p950 | |"
gset job_nice 0; gset job_ionice none
read -r tps1 avg1 p951 n1 <<<"$(run uncapped)"
d() { awk -v a="$1" -v b="$2" 'BEGIN { if (b > 0) printf "%+.1f %%", (a - b) * 100 / b; else print "-" }'; }
echo "| uncapped (nice 0, ionice none) | $n1 | $tps1 | $(d "$tps1" "$tps0") | $avg1 | $p951 | $(d "$p951" "$p950") |"
P -c "ALTER SYSTEM RESET pgbx.job_nice" -c "ALTER SYSTEM RESET pgbx.job_ionice" -c "SELECT pg_reload_conf()" >/dev/null
read -r tps2 avg2 p952 n2 <<<"$(run capped)"
echo "| capped (defaults: nice 10, best-effort-7) | $n2 | $tps2 | $(d "$tps2" "$tps0") | $avg2 | $p952 | $(d "$p952" "$p950") |"
echo "" ; echo "host: $(uname -sm), $cores core(s) in the container; pg $(P -c 'SHOW server_version'); dump $(P -c 'SHOW pgbx.dump_compression')"
P -c "DROP DATABASE IF EXISTS bench WITH (FORCE)" >/dev/null
