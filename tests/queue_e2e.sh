#!/usr/bin/env bash
# The server-wide job queue (ADR 0001 §0) against docker/compose.test.yml (run after tests/e2e.sh; needs the server up).
# Slow jobs are made with pgbx.upload_kbps. bash 3.2-safe; exits non-zero on any failure.
#   1. the worker keeps polling while a job runs; restore lane; max_concurrent_jobs; pick order; why a job waits
#      (server_queue, status(), pgbx jobs) and who may see it
#   2. overrun_policy=skip: a dump longer than its interval skips slots (params.skipped_slots), never back to back
#   3. cancel() of RUNNING jobs: backup (upload aborted, nothing in S3, no alert) and restore (half-restored db dropped)
#   4. time estimates: NOTICE on create, live progress, job_eta(), within +-50 % after 3 runs, capacity, doctor
#   5. crash: SIGTERM restart and kill -9 of the worker mid-upload -> job failed, no dump and no multipart upload left
set -u
cd "$(dirname "$0")/../docker"
DC="docker compose -f compose.test.yml"
P() { $DC exec -T db psql -U postgres -v ON_ERROR_STOP=1 -qAt "$@"; }
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "  PASS $1"; pass=$((pass+1)); else echo "  FAIL $1 (got '$2', want '$3')"; fail=$((fail+1)); fi; }
state() { P -d "$1" -c "SELECT state FROM pgbx.history WHERE id=$2"; }
wait_state() { # db id want timeout_s -> final state
  local s=""; for _ in $(seq "$4"); do s=$(state "$1" "$2"); case " $3 " in *" $s "*) break;; esac; sleep 1; done; echo "$s"; }
wait_end() { wait_state "$1" "$2" "done failed cancelled" "${3:-300}"; }
wait_up() { local ok=0; for _ in $(seq 120); do if P -c "SELECT 1" >/dev/null 2>&1; then ok=$((ok+1)); [ $ok -ge 3 ] && return 0; else ok=0; fi; sleep 1; done; return 1; }
gset() { P -c "ALTER SYSTEM SET pgbx.$1 = '$2'" -c "SELECT pg_reload_conf()" >/dev/null; sleep 1; }
greset() { for g in "$@"; do P -c "ALTER SYSTEM RESET pgbx.$g" >/dev/null; done; P -c "SELECT pg_reload_conf()" >/dev/null; sleep 1; }
# multipart uploads still open under this server's folder (aws-cli container; keys read from the credentials file, never printed)
open_uploads() {
  docker run --rm -v "$PWD/test.credentials:/c:ro" -e EP="$(P -c 'SHOW pgbx.s3_endpoint')" -e BK="$(P -c 'SHOW pgbx.s3_bucket')" \
    -e PFX="$(P -c 'SHOW pgbx.server_name')/" --entrypoint sh amazon/aws-cli -c '
    export AWS_ACCESS_KEY_ID=$(sed -n "s/^ *access_key_id *= *//p" /c) AWS_SECRET_ACCESS_KEY=$(sed -n "s/^ *secret_access_key *= *//p" /c) AWS_DEFAULT_REGION=us-east-1
    aws --endpoint-url "$EP" s3api list-multipart-uploads --bucket "$BK" --prefix "$PFX" --output json 2>/dev/null | grep -c "\"UploadId\""' 2>/dev/null
}
dumps() { P -d "$1" -c "SELECT count(*) FROM pgbx.history WHERE kind='backup' AND state='done'"; }
s3_dumps() { $DC exec -T -u postgres db pgbx backups --from-s3 --db "$1" --s3-endpoint "$(P -c 'SHOW pgbx.s3_endpoint')" \
  --s3-bucket "$(P -c 'SHOW pgbx.s3_bucket')" --s3-region "$(P -c 'SHOW pgbx.s3_region')" --server-name "$(P -c 'SHOW pgbx.server_name')" \
  --credentials-file /etc/pgbx/s3.credentials --json 2>/dev/null | jq '.backups | length'; }

wait_up || { echo "server not up"; exit 1; }
echo "## setup: qa (~20 MB dump, multipart), qb, both with a first backup"
for d in qa qb qc qb_restored qb_restored2; do P -c "DROP DATABASE IF EXISTS $d WITH (FORCE)" >/dev/null; done
P -c "CREATE DATABASE qa" -c "CREATE DATABASE qb" >/dev/null
P -d qa -c "CREATE TABLE t AS SELECT g AS id, md5(g::text) || md5((g*7)::text) || md5((g*13)::text) AS a FROM generate_series(1,500000) g" >/dev/null
P -d qb -c "CREATE TABLE t AS SELECT g AS id FROM generate_series(1,1000) g" >/dev/null
for d in qa qb; do for _ in $(seq 120); do [ "$(P -d $d -c "SELECT count(*) FROM pgbx.history WHERE kind='backup' AND state='done'" 2>/dev/null)" -ge 1 ] 2>/dev/null && break; sleep 1; done; done
id=$(P -d qa -c "SELECT pgbx.backup_now()"); check "uncapped qa backup (has the table)" "$(wait_end qa "$id")" done
id=$(P -d qb -c "SELECT pgbx.backup_now()"); check "uncapped qb backup" "$(wait_end qb "$id")" done
qa_bytes=$(P -d qa -c "SELECT bytes FROM pgbx.history WHERE id=(SELECT max(id) FROM pgbx.history WHERE kind='backup' AND state='done')")
echo "  qa dump: $qa_bytes bytes"
check "qa dump is multipart (> 16 MiB)" "$([ "${qa_bytes:-0}" -gt 16777216 ] && echo yes || echo "no ($qa_bytes)")" yes

echo "## 1. the worker keeps polling while a job runs; restore lane; max_concurrent_jobs; pick order; why a job waits"
gset upload_kbps 256
slow=$(P -d qa -c "SELECT pgbx.backup_now()")
check "slow qa backup running" "$(wait_state qa "$slow" "running" 30)" running
P -c "CREATE DATABASE qc" >/dev/null
for _ in $(seq 20); do n=$(P -d qc -c "SELECT count(*) FROM pgbx.history WHERE kind='backup' AND trigger='first'" 2>/dev/null); [ "${n:-0}" -ge 1 ] && break; sleep 1; done
check "new database qc found and its first backup queued while qa dumps" "${n:-0}" 1
check "qc in the overview" "$(P -c "SELECT count(*) FROM pgbx.overview() WHERE database='qc'")" 1
check "one slot: only one job runs" "$(P -d qc -c "SELECT state FROM pgbx.history WHERE kind='backup' AND trigger='first'")" queued
check "restore_lane on by default" "$(P -c 'SHOW pgbx.restore_lane')" on
lid=$(P -d qb -c "SELECT pgbx.restore(into_db => 'qb_restored')")
check "restore lane: qb's restore runs and finishes while qa's dump holds the only slot" "$(wait_end qb "$lid" 60)|$(state qa "$slow")" "done|running"
check "restored" "$(P -d qb_restored -c 'SELECT count(*) FROM t')" 1000
check "server_queue: qa's dump in slot 1" "$(P -c "SELECT state||'|'||slot||'|'||detail FROM pgbx.server_queue WHERE database='qa' AND job_id=$slow")" "running|1|running in job slot 1"
gset restore_lane off
rid=$(P -d qb -c "SELECT pgbx.restore(into_db => 'qb_restored2')")
vid=$(P -d qb -c "SELECT pgbx.verify_now()")
sleep 6
check "restore_lane off: the restore waits for the slot" "$(state qb "$rid")" queued
check "qa still running (not blocked by the queue)" "$(state qa "$slow")" running
check "server_queue: restore is #1 in line and says why" \
  "$(P -c "SELECT state||'|'||position||'|'||detail FROM pgbx.server_queue WHERE database='qb' AND job_id=$rid")" \
  "queued|1|waits for a job slot: 1 of 1 in use (qa backup #$slow)"
check "status() in qb says why its next job waits" \
  "$(P -d qb -c "SELECT next_job||'|'||queue_position||'|'||waiting_reason FROM pgbx.status()")" \
  "$rid|1|waits for a job slot: 1 of 1 in use (qa backup #$slow)"
check "status() in qa: running_job" "$(P -d qa -c "SELECT running_job FROM pgbx.status()")" "$slow"
check "job_eta() of the queued restore: #1 in line, a start and a finish" \
  "$(P -d qb -c "SELECT queue_position||'|'||(progress LIKE 'queued, #1 in line: starts ~%')||'|'||(eta_finish > eta_start) FROM pgbx.job_eta($rid)")" "1|true|true"
check "server_queue: the running dump reports progress" "$(P -c "SELECT progress ~ '^[0-9]+ % · ~' FROM pgbx.server_queue WHERE database='qa' AND job_id=$slow")" t
out=$($DC exec -T -u postgres db pgbx jobs --json 2>/dev/null)
check "pgbx jobs --json: qa running, qb restore queued #1" \
  "$(echo "$out" | jq -r --arg s "$slow" --arg r "$rid" '[(.jobs[]|select(.database=="qa" and (.job_id|tostring)==$s)|.state), (.jobs[]|select(.database=="qb" and (.job_id|tostring)==$r)|"\(.state) \(.position)")]|join("|")')" \
  "running|queued 1"
check "pgbx jobs (text)" "$($DC exec -T -u postgres db pgbx jobs 2>/dev/null | grep -c "#1 in line")" 1
P -c "DROP ROLE IF EXISTS q_viewer" -c "DROP ROLE IF EXISTS q_plain" -c "CREATE ROLE q_viewer LOGIN IN ROLE pgbx_viewer" -c "CREATE ROLE q_plain LOGIN" >/dev/null
AS() { u=$1; shift; $DC exec -T db psql -U "$u" -qAt "$@" 2>&1; }
check "viewer reads server_queue" "$(AS q_viewer -d postgres -c "SELECT count(*) > 0 FROM pgbx.server_queue")" t
check "plain role cannot read server_queue" "$(AS q_plain -d postgres -c "SELECT count(*) FROM pgbx.server_queue" | grep -c 'permission denied')" 1
check "viewer cannot cancel" "$(AS q_viewer -d qb -c "SELECT pgbx.cancel($rid)" | grep -c 'permission denied')" 1
P -c "DROP ROLE q_viewer" -c "DROP ROLE q_plain" >/dev/null
gset max_concurrent_jobs 2
r=$(wait_state qb "$rid" "running done" 20); qs=$(state qa "$slow")
check "max_concurrent_jobs=2: qb restore starts while qa still dumps" "$(echo "$r" | grep -cE '^(running|done)$')|$qs" "1|running"
check "restore done" "$(wait_end qb "$rid" 120)" done
qcid=$(P -d qc -c "SELECT id FROM pgbx.history WHERE kind='backup' AND trigger='first'")
check "then qc's first backup and qb's restore test" "$(wait_end qc "$qcid" 120)|$(wait_end qb "$vid" 120)" "done|done"
# one free slot: started in pick order (restore > scheduled/first backup > restore test), not in queue order
r=$(P -d qb -c "SELECT (SELECT started FROM pgbx.history WHERE id=$rid) < '$(P -d qc -c "SELECT started FROM pgbx.history WHERE id=$qcid")'::timestamptz
                AND '$(P -d qc -c "SELECT started FROM pgbx.history WHERE id=$qcid")'::timestamptz < (SELECT started FROM pgbx.history WHERE id=$vid)")
check "pick order: restore, then first backup, then restore test" "$r" t
check "advisory slots held = 1 (only qa runs)" "$(P -c "SELECT count(*) FROM pg_locks WHERE locktype='advisory' AND objsubid=2 AND granted
                                                      AND database=(SELECT oid FROM pg_database WHERE datname=current_database())")" 1
greset max_concurrent_jobs restore_lane
check "slow qa backup done" "$(wait_end qa "$slow" 300)" done

echo "## 2. overrun_policy=skip: an every-minute schedule with ~90 s dumps"
check "default overrun_policy" "$(P -c 'SHOW pgbx.overrun_policy')|$(P -c 'SHOW pgbx.overrun_max_gap')" "skip|1.5"
P -d qa -c "SELECT pgbx.set_schedule('every 1 minute')" >/dev/null
# the first scheduled run after this, then the one after it
for _ in $(seq 150); do s1=$(P -d qa -c "SELECT id FROM pgbx.history WHERE kind='backup' AND trigger='schedule' ORDER BY id LIMIT 1"); [ -n "$s1" ] && break; sleep 1; done
check "a scheduled backup runs" "$(wait_end qa "$s1" 300)" done
for _ in $(seq 120); do s2=$(P -d qa -c "SELECT id FROM pgbx.history WHERE kind='backup' AND trigger='schedule' AND id > $s1 ORDER BY id LIMIT 1"); [ -n "$s2" ] && break; sleep 1; done
r=$(P -d qa -c "SELECT (extract(epoch FROM a.finished - a.started) > 60)::text || '|' || (coalesce(b.params->>'skipped_slots', '0')::int >= 1)::text || '|' ||
               (b.requested_at >= date_trunc('minute', a.finished) + interval '1 minute')::text
               FROM pgbx.history a, pgbx.history b WHERE a.id = $s1 AND b.id = $s2")
check "dump > 1 min; next run records skipped_slots and waits for the next slot after it finished" "$r" "true|true|true"
P -d qa -c "SELECT pgbx.set_schedule('daily at 02:00')" >/dev/null
check "that one runs too" "$(wait_end qa "$s2" 300)" done

echo "## 3. cancel() of running jobs"
before=$(dumps qa); s3before=$(s3_dumps qa)
$DC exec -T -u postgres db sh -c ': > /var/lib/postgresql/alerts.log'
id=$(P -d qa -c "SELECT pgbx.backup_now()"); wait_state qa "$id" running 30 >/dev/null; sleep 4
check "multipart upload open while it runs" "$(open_uploads)" 1
out=$($DC exec -T -u postgres db pgbx jobs cancel "$id" --json 2>/dev/null)
check "pgbx jobs cancel without --yes refused" "$(echo "$out" | jq -r '"\(.ok) \(.error|test("--yes"))"')" "false true"
out=$($DC exec -T -u postgres db pgbx jobs cancel "$id" --yes --json 2>/dev/null)
check "pgbx jobs cancel ID --yes (database found in the queue)" "$(echo "$out" | jq -r '"\(.ok) \(.database)"')" "true qa"
t0=$SECONDS; s=$(wait_end qa "$id" 30)
check "running backup cancelled within seconds" "$s|$([ $((SECONDS-t0)) -le 15 ] && echo fast || echo "slow $((SECONDS-t0))s")" "cancelled|fast"
check "error says who" "$(P -d qa -c "SELECT error FROM pgbx.history WHERE id=$id")" "cancelled by postgres while running"
check "no multipart upload left" "$(open_uploads)" 0
check "no dump in history or S3" "$(dumps qa)|$(s3_dumps qa)" "$before|$s3before"
check "no alert for a cancel" "$($DC exec -T db cat /var/lib/postgresql/alerts.log | grep -c "\"job_id\":$id,")" 0
check "pgbx_dump process gone" "$(P -c "SELECT count(*) FROM pg_stat_activity WHERE application_name='pgbx_dump' AND datname='qa'")" 0
gset download_kbps 256
P -c "DROP DATABASE IF EXISTS qa_r WITH (FORCE)" >/dev/null
id=$(P -d qa -c "SELECT pgbx.restore(into_db => 'qa_r')"); wait_state qa "$id" running 30 >/dev/null; sleep 4
check "restore writing qa_r" "$(P -c "SELECT count(*) FROM pg_database WHERE datname='qa_r'")" 1
msg=$(P -d qa -c "SELECT pgbx.cancel($id)")
check "cancel() of a running restore" "$(echo "$msg" | grep -c 'is running: the worker stops it')" 1
check "restore cancelled" "$(wait_end qa "$id" 30)" cancelled
check "half-restored database dropped" "$(P -c "SELECT count(*) FROM pg_database WHERE datname='qa_r'")" 0
bad=$(P -d qa -c "SELECT pgbx.cancel($id)" 2>&1); check "cancel of a cancelled job refused" "$(echo "$bad" | grep -c 'only a queued or running job')" 1
greset download_kbps

echo "## 4. time estimates"
gset upload_kbps 1024
out=$($DC exec -T db psql -U postgres -d qa -qAt -c "SELECT pgbx.backup_now()" 2>&1)
id=$(echo "$out" | grep -E '^[0-9]+$'); echo "  $(echo "$out" | grep NOTICE)"
check "NOTICE on create: when it starts, how long, how big, why" "$(echo "$out" | grep -cE "backup job $id queued.*starts ~.*takes ~.*confidence")" 1
mid=no; prog=""; for _ in $(seq 90); do
  r=$(P -d qa -c "SELECT h.state||'|'||coalesce(j.done_bytes,0)||'|'||coalesce(j.progress,'') FROM pgbx.history h, pgbx.job_eta(h.id) j WHERE h.id=$id")
  st=${r%%|*}; rest=${r#*|}; d=${rest%%|*}
  [ "$st" = running ] && [ "$d" -gt 0 ] && { mid=yes; prog=${rest#*|}; }
  [ "$st" = done ] && break; sleep 1; done
echo "  while running: $prog"
check "live progress while running (done_bytes > 0, 'N % · ~T left')" "$mid|$(echo "$prog" | grep -cE '^[0-9]+ % · ~')" "yes|1"
check "finished" "$st" done
for n in 2 3 4; do id=$(P -d qa -c "SELECT pgbx.backup_now()" 2>/dev/null); s=$(wait_end qa "$id" 120); done
r=$(P -d qa -c "SELECT round(extract(epoch FROM finished - started))||'|'||(params->>'eta_sec')||'|'||(abs(extract(epoch FROM finished - started) - (params->>'eta_sec')::float8) <= 0.5 * extract(epoch FROM finished - started)) FROM pgbx.history WHERE id=$id")
echo "  4th capped backup: actual|estimate = ${r%|*} s ($(P -d qa -c "SELECT params->>'eta_basis' FROM pgbx.history WHERE id=$id"))"
check "after 3 runs the estimate is within +-50 % of the actual" "$s|${r##*|}" "done|true"
check "capacity measured (cpu probe, upload speed)" "$(P -d qa -c "SELECT (cpu_bps > 0)||'|'||(upload_bps > 0) FROM pgbx.server_capacity")" "true|true"
check "doctor(): capacity and eta_accuracy rows" "$(P -c "SELECT count(*) FROM pgbx.doctor() WHERE name IN ('capacity', 'eta_accuracy')")" 2
greset upload_kbps

echo "## 5. crash: the job never leaves a dump or an open multipart upload behind"
gset upload_kbps 256
before=$(dumps qa); s3before=$(s3_dumps qa)
id=$(P -d qa -c "SELECT pgbx.backup_now()"); wait_state qa "$id" running 30 >/dev/null; sleep 4
check "multipart upload open while it runs" "$(open_uploads)" 1
$DC restart -t 30 db >/dev/null 2>&1; wait_up
for _ in $(seq 30); do s=$(state qa "$id"); [ "$s" = failed ] && break; sleep 1; done
check "SIGTERM restart mid-upload: job failed" "$s" failed
echo "  error: $(P -d qa -c "SELECT error FROM pgbx.history WHERE id=$id")"
sleep 5; check "no multipart upload left" "$(open_uploads)" 0
check "no new dump in history or S3" "$(dumps qa)|$(s3_dumps qa)" "$before|$s3before"
id=$(P -d qa -c "SELECT pgbx.backup_now()"); wait_state qa "$id" running 30 >/dev/null; sleep 4
# the scheduler by its process title (it has no database session of its own)
pid=$($DC exec -T db sh -c 'for p in /proc/[0-9]*; do tr "\0" " " < $p/cmdline 2>/dev/null | grep -q "^postgres: pgbx scheduler" && echo ${p#/proc/}; done' | head -1)
check "found the scheduler process" "$([ -n "$pid" ] && echo yes)" yes
since=$(date -u +%Y-%m-%dT%H:%M:%SZ)
$DC exec -T db sh -c "kill -9 $pid"; sleep 3; wait_up
for _ in $(seq 60); do s=$(state qa "$id" 2>/dev/null); [ "$s" = failed ] && break; sleep 1; done
check "kill -9 of the worker mid-upload: job failed (interrupted)" "$s|$(P -d qa -c "SELECT error LIKE 'interrupted%' FROM pgbx.history WHERE id=$id")" "failed|t"
for _ in $(seq 30); do n=$($DC logs --since "$since" db 2>&1 | grep -c "aborted orphaned upload"); [ "$n" -ge 1 ] && break; sleep 1; done
check "the restarted worker aborted the orphaned upload" "$n" 1
check "no multipart upload left" "$(open_uploads)" 0
check "no new dump in history or S3" "$(dumps qa)|$(s3_dumps qa)" "$before|$s3before"
greset upload_kbps
id=$(P -d qa -c "SELECT pgbx.backup_now()"); check "backups work after the crash" "$(wait_end qa "$id" 120)" done
for d in qa qb qc qb_restored qb_restored2; do P -c "DROP DATABASE IF EXISTS $d WITH (FORCE)" >/dev/null; done
echo "== queue_e2e: $pass passed, $fail failed"
[ $fail -eq 0 ]
