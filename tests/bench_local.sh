#!/usr/bin/env bash
# Local speed + chaos bench (docker/compose.local.yml): ~1.2 GB database, streaming backup/restore timings,
# then S3 is stopped for 10 s in the middle of a backup and of a restore — both must still finish correctly.
set -u
cd "$(dirname "$0")/../docker"
DC="docker compose -f compose.local.yml"
P() { $DC exec -T db psql -U postgres -v ON_ERROR_STOP=1 -qAt "$@"; }
job() { P -d big -c "SELECT state||'|'||coalesce(bytes,0)||'|'||coalesce(extract(epoch FROM finished-started)::numeric(10,1),0)||'|'||coalesce(error,'') FROM pgbx.history WHERE id=$1"; }
wait_job() { for _ in $(seq 900); do s=$(job "$1"); case "$s" in done*|failed*) echo "$s"; return;; esac; sleep 1; done; echo "timeout|$s"; }
mbps() { awk -v b="$1" -v s="$2" 'BEGIN{ if (s>0) printf "%.0f MB/s", b/1048576/s; else print "-" }'; }
pass=0; fail=0; check() { if [ "$2" = "$3" ]; then echo "  PASS $1"; pass=$((pass+1)); else echo "  FAIL $1 (got '$2', want '$3')"; fail=$((fail+1)); fi; }

$DC up -d 2>&1 | tail -1
for _ in $(seq 60); do P -c "SELECT 1" >/dev/null 2>&1 && break; sleep 1; done

echo "## build a ~1.2 GB database"
P -c "DROP DATABASE IF EXISTS big WITH (FORCE)" -c "DROP DATABASE IF EXISTS big_r1 WITH (FORCE)" -c "DROP DATABASE IF EXISTS big_r2 WITH (FORCE)"
P -c "DROP DATABASE IF EXISTS big_r3 WITH (FORCE)" -c "CREATE DATABASE big"
P -d big -c "SELECT pgbx.pause('loading')" >/dev/null 2>&1 || sleep 5
P -d big -c "SELECT pgbx.pause('loading')" >/dev/null
t=$SECONDS
P -d big -c "CREATE TABLE t AS SELECT g AS id, md5(g::text)||md5((g+1)::text) AS a, md5((g*3)::text) AS b, now() AS ts FROM generate_series(1,9000000) g"
size=$(P -d big -c "SELECT pg_database_size('big')"); rows=9000000
echo "  loaded $(P -d big -c "SELECT pg_size_pretty(pg_database_size('big'))") in $((SECONDS-t))s"

echo "## 1. streaming backup (pg_dump --compress zstd:3 -> 16 MB multipart parts)"
id=$(P -d big -c "SELECT pgbx.backup_now()"); r=$(wait_job "$id"); IFS='|' read -r st bytes secs err <<<"$r"
echo "  backup: $st, dump $(numfmt --to=iec "$bytes" 2>/dev/null || echo "$bytes") in ${secs}s = $(mbps "$size" "$secs") of database"
check "backup done" "$st" done

echo "## 2. streaming restore (S3 -> pg_restore stdin, no temp file)"
id=$(P -d big -c "SELECT pgbx.restore(into_db => 'big_r1')"); r=$(wait_job "$id"); IFS='|' read -r st bytes secs err <<<"$r"
echo "  restore: $st in ${secs}s = $(mbps "$size" "$secs") of database ${err:+($err)}"
check "restore done" "$st" done
check "all rows restored" "$(P -d big_r1 -c 'SELECT count(*) FROM t')" "$rows"

echo "## 3. chaos: S3 down for 10 s during a backup"
id=$(P -d big -c "SELECT pgbx.backup_now()"); sleep 4
$DC stop s3 >/dev/null 2>&1; echo "  S3 stopped at +4s"; sleep 10; $DC start s3 >/dev/null 2>&1; echo "  S3 back at +14s"
r=$(wait_job "$id"); IFS='|' read -r st bytes secs err <<<"$r"; echo "  backup: $st in ${secs}s ${err:+($err)}"
check "backup survived S3 outage" "$st" done
echo "  retries logged: $($DC logs db 2>&1 | grep -c 'retry [0-9]')"
id=$(P -d big -c "SELECT pgbx.restore(into_db => 'big_r2')"); r=$(wait_job "$id"); IFS='|' read -r st bytes secs err <<<"$r"
check "that backup restores completely" "$(P -d big_r2 -c 'SELECT count(*) FROM t' 2>/dev/null)" "$rows"

echo "## 4. chaos: S3 down for 10 s during a restore (resume from the byte reached)"
id=$(P -d big -c "SELECT pgbx.restore(into_db => 'big_r3')"); sleep 4
$DC stop s3 >/dev/null 2>&1; echo "  S3 stopped at +4s"; sleep 10; $DC start s3 >/dev/null 2>&1; echo "  S3 back at +14s"
r=$(wait_job "$id"); IFS='|' read -r st bytes secs err <<<"$r"; echo "  restore: $st in ${secs}s ${err:+($err)}"
check "restore survived S3 outage" "$st" done
check "all rows after resumed download" "$(P -d big_r3 -c 'SELECT count(*) FROM t' 2>/dev/null)" "$rows"
check "same data (checksum)" "$(P -d big_r3 -c "SELECT md5(string_agg(a, '' ORDER BY id)) FROM t WHERE id % 1000 = 0")" "$(P -d big -c "SELECT md5(string_agg(a, '' ORDER BY id)) FROM t WHERE id % 1000 = 0")"
echo "  resumes logged: $($DC logs db 2>&1 | grep -c 'download .* retry')"

echo "## 5. Postgres stop during a backup must not wait for it"
id=$(P -d big -c "SELECT pgbx.backup_now()"); sleep 3
t0=$(date +%s); $DC stop -t 60 db >/dev/null 2>&1; took=$(( $(date +%s) - t0 ))
echo "  Postgres stopped ${took}s after the request (backup was mid-upload)"
check "shutdown not blocked by the backup (<10s)" "$([ $took -lt 10 ] && echo yes || echo "no (${took}s)")" yes
$DC start db >/dev/null 2>&1; for _ in $(seq 60); do P -c "SELECT 1" >/dev/null 2>&1 && break; sleep 1; done
for _ in $(seq 20); do st=$(P -d big -c "SELECT state FROM pgbx.history WHERE id=$id"); [ "$st" != running ] && break; sleep 1; done
check "interrupted job marked failed, not stuck" "$st" failed
id=$(P -d big -c "SELECT pgbx.backup_now()"); r=$(wait_job "$id"); check "next backup works" "${r%%|*}" done

echo "RESULT: $pass passed, $fail failed"; [ $fail -eq 0 ]
