#!/usr/bin/env bash
# Quiet-window suggestion (ADR 0001 §2) and the load gate (§1) against docker/compose.test.yml (needs the server up).
# bash 3.2-safe; exits non-zero on any failure.
#   1. suggest_window() on synthetic histograms (daily, weekday split, low confidence, avoiding other databases' slots),
#      status(), doctor schedule_in_quiet_window, pgbx schedule suggest [--apply]
#   2. load gate: shadow never delays but records would_defer; NOTICE for human jobs; per-database on defers a
#      scheduled backup, then forces it at max_defer; when the load stops it runs before the deadline, not forced;
#      pgbx load
set -u
cd "$(dirname "$0")/../docker"
DC="docker compose -f compose.test.yml"
P() { $DC exec -T db psql -U postgres -v ON_ERROR_STOP=1 -qAt "$@"; }
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "  PASS $1"; pass=$((pass+1)); else echo "  FAIL $1 (got '$2', want '$3')"; fail=$((fail+1)); fi; }
J() { $DC exec -T -u postgres db pgbx "$@" --json 2>/dev/null; }
gset() { P -c "ALTER SYSTEM SET pgbx.$1 = '$2'" -c "SELECT pg_reload_conf()" >/dev/null; sleep 1; }
greset() { for g in "$@"; do P -c "ALTER SYSTEM RESET pgbx.$g" >/dev/null; done; P -c "SELECT pg_reload_conf()" >/dev/null; sleep 1; }
state() { P -d "$1" -c "SELECT state FROM pgbx.history WHERE id=$2"; }
wait_end() { local s=""; for _ in $(seq "${3:-300}"); do s=$(state "$1" "$2"); case "$s" in done|failed|cancelled) break;; esac; sleep 1; done; echo "$s"; }
# synthetic server-wide histogram in the admin database: <dow-expr> <hour-expr> give xacts per bucket
synth() { # samples xacts-sql
  P -c "DELETE FROM pgbx.activity_hourly WHERE scope = 'server'" \
    -c "INSERT INTO pgbx.activity_hourly (scope, dow, hour, samples, xacts, writes, reads, active_max, updated_at)
        SELECT 'server', d, h, $1, x, x / 10, 0, x / 1000, now() FROM generate_series(0, 6) d, generate_series(0, 23) h,
               LATERAL (SELECT ($2)::float8 AS x) v" >/dev/null; }
newdb() { P -c "DROP DATABASE IF EXISTS $1 WITH (FORCE)" -c "CREATE DATABASE $1" >/dev/null
  for _ in $(seq 60); do [ "$(P -d "$1" -c "SELECT count(*) FROM pgbx.activity_hourly WHERE scope='server'" 2>/dev/null)" = 168 ] && return; sleep 1; done; }

P -c "SELECT 1" >/dev/null || { echo "server not up"; exit 1; }
echo "## 1. quiet window"
# busy 08-20 UTC every day, medium otherwise, quietest at 03:00; 8 samples per bucket = 56 days
synth 8 "CASE WHEN h BETWEEN 8 AND 20 THEN 10000 WHEN h = 3 THEN 50 ELSE 1000 END"
newdb win
check "the worker copies the server-wide histogram into a new database" "$(P -d win -c "SELECT count(*) FROM pgbx.activity_hourly WHERE scope='server'")" 168
r=$(P -d win -c "SELECT cron||'|'||confidence||'|'||(score < current_score)||'|'||apply_sql FROM pgbx.suggest_window()")
check "daily quiet hour 03:00 UTC, high confidence, quieter than 02:00" "$r" "0 3 * * *|high|true|SELECT pgbx.configure(schedule => '0 3 * * *');"
check "status() shows it, never applied by itself" "$(P -d win -c "SELECT suggested_schedule LIKE '0 3 * * *%never applied by itself%' FROM pgbx.status()")" t
P -d win -c "SELECT pgbx.set_schedule('daily at 12:00')" >/dev/null
# (other databases may be listed too: cli_e2e leaves shop on 'every 2 hours', which hits busy hours)
listed() { P -c "SELECT ok||'|'||(detail LIKE '%win: \"' || \$\$$1\$\$ || '\" runs at %; 0 3 * * * would be %') FROM pgbx.doctor() WHERE name='schedule_in_quiet_window'"; }
for _ in $(seq 20); do r=$(listed 'daily at 12:00'); [ "$r" = "false|true" ] && break; sleep 1; done
check "doctor(): schedule_in_quiet_window warns about win at 12:00 and names 0 3 * * *" "$r" "false|true"
P -c "SELECT detail, fix FROM pgbx.doctor() WHERE name='schedule_in_quiet_window'" | sed 's/^/  /' | cut -c1-220
out=$(J schedule suggest --db win)
check "pgbx schedule suggest --json" "$(echo "$out" | jq -r '"\(.ok) \(.suggestion.cron) \(.applied) \(.safety)"')" "true 0 3 * * * false readonly"
out=$(J schedule suggest --db win --apply)
check "--apply without --yes (no terminal) refused" "$(echo "$out" | jq -r '"\(.ok) \(.error|test("--yes"))"')" "false true"
check "nothing changed" "$(P -d win -c "SELECT cron FROM pgbx.status()")" "0 12 * * *"
out=$(J schedule suggest --db win --apply --yes)
check "--apply --yes applies it" "$(echo "$out" | jq -r '"\(.ok) \(.applied)"')|$(P -d win -c "SELECT cron FROM pgbx.status()")" "true true|0 3 * * *"
for _ in $(seq 20); do r=$(P -c "SELECT position('win:' IN coalesce(detail, '')) = 0 FROM pgbx.doctor() WHERE name='schedule_in_quiet_window'"); [ "$r" = t ] && break; sleep 1; done
check "doctor(): win no longer listed" "$r" t
# another database's backup starts at 03:00 -> the suggestion for a third one moves away from that hour
newdb win2
P -d win -c "SELECT pgbx.set_schedule('daily at 03:00')" >/dev/null; sleep 8
r=$(P -d win2 -c "SELECT cron FROM pgbx.suggest_window()")
check "an hour another database's backup starts in is avoided" "$([ "$r" != "0 3 * * *" ] && [ -n "$r" ] && echo moved)" moved
check "viewer may call suggest_window()" "$(P -c "DROP ROLE IF EXISTS w_viewer" -c "CREATE ROLE w_viewer LOGIN IN ROLE pgbx_viewer" >/dev/null;
  $DC exec -T db psql -U w_viewer -d win -qAt -c "SELECT cron IS NOT NULL FROM pgbx.suggest_window()" 2>&1)" t
P -c "DROP ROLE w_viewer" >/dev/null
# weekdays 3x busier than the weekend -> a weekly slot on the weekend
synth 8 "CASE WHEN d IN (0, 6) THEN 300 ELSE 3000 END + CASE WHEN h = 5 THEN 0 ELSE 50 END"
newdb winw
r=$(P -d winw -c "SELECT cron||'|'||start_at FROM pgbx.suggest_window()")
check "weekday split: a weekly weekend slot" "$(echo "$r" | grep -cE '^0 5 \* \* [06]\|(sunday|saturday) 05:00 UTC$')" 1
synth 1 "CASE WHEN h = 3 THEN 50 ELSE 1000 END"
P -c "DELETE FROM pgbx.activity_hourly WHERE scope = 'server' AND dow <> 0" >/dev/null   # one day of samples
P -c "DROP DATABASE IF EXISTS winl WITH (FORCE)" -c "CREATE DATABASE winl" >/dev/null
for _ in $(seq 60); do [ "$(P -d winl -c "SELECT count(*) FROM pgbx.activity_hourly WHERE scope='server'" 2>/dev/null)" = 24 ] && break; sleep 1; done
check "one day of samples: low confidence" "$(P -d winl -c "SELECT confidence||'|'||days_sampled FROM pgbx.suggest_window()")" "low|1"
P -c "DELETE FROM pgbx.activity_hourly WHERE scope = 'server'" >/dev/null
P -c "DROP DATABASE IF EXISTS winn WITH (FORCE)" -c "CREATE DATABASE winn" >/dev/null
for _ in $(seq 30); do P -d winn -c "SELECT 1 FROM pgbx.config" >/dev/null 2>&1 && break; sleep 1; done
check "no samples at all: confidence none, nothing to apply" "$(P -d winn -c "SELECT confidence||'|'||(cron IS NULL) FROM pgbx.suggest_window()")" "none|true"
for d in win win2 winw winl winn; do P -c "DROP DATABASE IF EXISTS $d WITH (FORCE)" >/dev/null; done

echo "## 2. load gate (pgbench: 6 clients)"
check "default pgbx.load_gate = shadow" "$(P -c 'SHOW pgbx.load_gate')" shadow
P -c "DROP DATABASE IF EXISTS gate WITH (FORCE)" -c "DROP DATABASE IF EXISTS gate2 WITH (FORCE)" -c "CREATE DATABASE gate" -c "CREATE DATABASE gate2" >/dev/null
$DC exec -T db pgbench -U postgres -i -s 5 -q gate >/dev/null 2>&1
for d in gate gate2; do for _ in $(seq 60); do [ "$(P -d $d -c "SELECT count(*) FROM pgbx.history WHERE kind='backup' AND state='done'" 2>/dev/null)" -ge 1 ] 2>/dev/null && break; sleep 1; done; done
gset busy_active_backends 2; gset busy_tps 400; gset max_defer 100; gset defer_backoff 1
start_load() { $DC exec -T -d db pgbench -U postgres -c 6 -j 2 -T 900 gate >/dev/null 2>&1; }
stop_load() { P -c "SELECT count(pg_terminate_backend(pid)) FROM pg_stat_activity WHERE application_name = 'pgbench'" >/dev/null; }
start_load
for _ in $(seq 30); do b=$(J load | jq -r '.sample.load_busy'); [ "$b" = true ] && break; sleep 1; done
check "pgbx load: busy" "$b" true
echo "  $(J load | jq -r '.sample.load_reasons')"
out=$($DC exec -T db psql -U postgres -d gate2 -qAt -c "SELECT pgbx.backup_now()" 2>&1)
id=$(echo "$out" | grep -E '^[0-9]+$')
check "human job: NOTICE that it competes with the app; it starts anyway" "$(echo "$out" | grep -c 'will compete with the app; it starts anyway')" 1
check "and it runs" "$(wait_end gate2 "$id" 60)" done
P -d gate2 -c "SELECT pgbx.set_schedule('every 1 minute')" >/dev/null
for _ in $(seq 90); do sid=$(P -d gate2 -c "SELECT id FROM pgbx.history WHERE kind='backup' AND trigger='schedule' ORDER BY id DESC LIMIT 1"); [ -n "$sid" ] && break; sleep 1; done
check "shadow: a scheduled backup under load is not delayed" "$(wait_end gate2 "$sid" 60)|$(P -d gate2 -c "SELECT started - requested_at < interval '15 seconds' FROM pgbx.history WHERE id=$sid")" "done|t"
check "shadow: would_defer recorded with the reason" "$(P -d gate2 -c "SELECT (params->>'would_defer')||'|'||(params->>'would_defer_reason' LIKE '%active sessions%' OR params->>'would_defer_reason' LIKE '%tps%')||'|'||coalesce(params->>'deferrals','none') FROM pgbx.history WHERE id=$sid")" "true|true|none"
P -d gate2 -c "SELECT pgbx.set_schedule('daily at 02:00')" >/dev/null
out=$(J load --gate on --db gate); check "pgbx load --gate on needs --yes" "$(echo "$out" | jq -r '"\(.ok) \(.error|test("--yes"))"')" "false true"
out=$(J load --gate on --db gate --yes); check "pgbx load --gate on --db gate --yes" "$(echo "$out" | jq -r '"\(.ok) \(.load_gate)"')" "true on"
check "status() shows the gate" "$(P -d gate -c "SELECT load_gate FROM pgbx.status()")" on
P -d gate -c "SELECT pgbx.set_schedule('every 2 minutes')" >/dev/null
for _ in $(seq 150); do sid=$(P -d gate -c "SELECT id FROM pgbx.history WHERE kind='backup' AND trigger='schedule' ORDER BY id DESC LIMIT 1"); [ -n "$sid" ] && break; sleep 1; done
for _ in $(seq 15); do r=$(P -d gate -c "SELECT state||'|'||coalesce(params->>'defer_reason','-') FROM pgbx.history WHERE id=$sid"); case "$r" in queued\|busy*) break;; esac; sleep 1; done
check "on: the scheduled backup is deferred (busy)" "$(echo "$r" | cut -c1-12)" "queued|busy:"
sleep 4
check "pgbx load lists it as deferred" "$(J load | jq -r --arg i "$sid" '[.deferred_jobs[]|select((.job_id|tostring)==$i)]|length')" 1
sleep 50; check "still queued after 50 s" "$(state gate "$sid")" queued
check "it ran at its deadline, forced" "$(wait_end gate "$sid" 120)|$(P -d gate -c "SELECT (params->>'forced')||'|'||(started <= (params->>'deadline')::timestamptz + interval '10 seconds')||'|'||((params->>'busy_deferrals')::int >= 1)::text FROM pgbx.history WHERE id=$sid")" "done|true|true|true"
# the next slot: deferred, then the load stops -> it runs before the deadline, not forced
for _ in $(seq 150); do s2=$(P -d gate -c "SELECT id FROM pgbx.history WHERE kind='backup' AND trigger='schedule' AND id > $sid ORDER BY id LIMIT 1"); [ -n "$s2" ] && break; sleep 1; done
for _ in $(seq 15); do r=$(P -d gate -c "SELECT state||'|'||coalesce(params->>'defer_reason','-') FROM pgbx.history WHERE id=$s2"); case "$r" in queued\|busy*) break;; esac; sleep 1; done
check "next one deferred too" "$(echo "$r" | cut -c1-12)" "queued|busy:"
stop_load
check "load stopped: it runs at its next retry, before the deadline, not forced" \
  "$(wait_end gate "$s2" 120)|$(P -d gate -c "SELECT coalesce(params->>'forced','no')||'|'||(started < (params->>'deadline')::timestamptz) FROM pgbx.history WHERE id=$s2")" "done|no|true"
for _ in $(seq 20); do b=$(J load | jq -r '.sample.load_busy'); [ "$b" = false ] && break; sleep 1; done
check "pgbx load: quiet again" "$b" false
out=$(J load --db gate)
check "pgbx load: per-database counts" "$(echo "$out" | jq -r '.databases[]|select(.database=="gate")|"\(.load_gate) \(.deferred_7d>=2) \(.forced_7d>=1)"')" "on true true"
check "doctor(): load_gate row" "$(P -c "SELECT ok||'|'||(detail LIKE '%on in gate%') FROM pgbx.doctor() WHERE name='load_gate'")" "true|true"
P -d gate -c "SELECT pgbx.configure(load_gate => 'default')" >/dev/null
greset busy_active_backends busy_tps max_defer defer_backoff
P -c "DROP DATABASE IF EXISTS gate WITH (FORCE)" -c "DROP DATABASE IF EXISTS gate2 WITH (FORCE)" >/dev/null
echo "== load_e2e: $pass passed, $fail failed"
[ $fail -eq 0 ]
