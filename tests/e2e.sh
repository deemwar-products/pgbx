#!/usr/bin/env bash
# End-to-end check against docker/compose.test.yml. Run: tests/e2e.sh
set -u
cd "$(dirname "$0")/../docker"
P() { docker compose -f compose.test.yml exec -T db psql -U postgres -v ON_ERROR_STOP=1 -qAt "$@"; }
wait_job() { # db id -> prints final "state key bytes error"
  for _ in $(seq 40); do s=$(P -d "$1" -c "SELECT state||' '||coalesce(s3_key,'-')||' '||coalesce(bytes::text,'-')||' '||coalesce(error,'') FROM pgbx.history WHERE id=$2"); case "$s" in done*|failed*) echo "$s"; return;; esac; sleep 1; done; echo "timeout: $s"; }
pass=0; fail=0; check() { if [ "$2" = "$3" ]; then echo "  PASS $1"; pass=$((pass+1)); else echo "  FAIL $1 (got '$2', want '$3')"; fail=$((fail+1)); fi; }

echo "## 1. create a database, do nothing else -> backed up automatically"
P -c "DROP DATABASE IF EXISTS shop" -c "DROP DATABASE IF EXISTS shop_restored" -c "CREATE DATABASE shop"
P -d shop -c "CREATE TABLE orders(id int primary key, item text); INSERT INTO orders SELECT g, 'item-'||g FROM generate_series(1,1000) g;"
t=$SECONDS; first=$(P -d shop -c "SELECT id FROM pgbx.history WHERE kind='backup' ORDER BY id LIMIT 1")
while [ -z "$first" ] && [ $((SECONDS-t)) -lt 120 ]; do sleep 1; first=$(P -d shop -c "SELECT id FROM pgbx.history WHERE kind='backup' ORDER BY id LIMIT 1"); done
r=$(wait_job shop "$first"); echo "  first backup after $((SECONDS-t))s: $r"; check "automatic first backup" "${r%% *}" done

echo "## 4. policy from a migration; bucket is server-wide"
check "configure()" "$(P -d shop -c "SELECT schedule||'|'||max_days FROM pgbx.configure(schedule => 'every 6 hours', max_days => 30)")" "0 */6 * * *|30"
echo "  server-wide: bucket=$(P -c 'SHOW pgbx.s3_bucket') server=$(P -c 'SHOW pgbx.server_name')"

echo "## 2. manual backup_now()"
id=$(P -d shop -c "SELECT pgbx.backup_now()"); r=$(wait_job shop "$id"); echo "  #$id: $r"; check "backup_now" "${r%% *}" done

echo "## 3. drop the table -> restore() into a new database"
sleep 1; P -d shop -c "DROP TABLE orders"
check "orders gone from shop" "$(P -d shop -c "SELECT count(*) FROM pg_tables WHERE tablename='orders'")" 0
id=$(P -d shop -c "SELECT pgbx.restore(into_db => 'shop_restored')"); r=$(wait_job shop "$id"); echo "  #$id: $r"; check "restore job" "${r%% *}" done
check "rows back in shop_restored" "$(P -d shop_restored -c 'SELECT count(*) FROM orders')" 1000
check "restored copy backs up to its own folder" "$(P -d shop_restored -c "SELECT coalesce((SELECT path FROM pgbx.config),'own')||'|'||(SELECT location FROM pgbx.status())")" "own|s3://$(P -c 'SHOW pgbx.s3_bucket')/$(P -c 'SHOW pgbx.server_name')/shop_restored/"

echo "## 5. friendly schedules"
for f in "every 1 hour|0 * * * *" "every 15 minutes|*/15 * * * *" "daily at 02:30|30 2 * * *" "weekly on sunday at 03:00|0 3 * * 0" "hourly|0 * * * *"; do
  check "to_cron('${f%%|*}')" "$(P -d shop -c "SELECT pgbx.to_cron('${f%%|*}')")" "${f##*|}"; done
echo "  $(P -d shop -c "SELECT pgbx.set_schedule('every 1 hour')")"
check "status shows new schedule" "$(P -d shop -c "SELECT schedule||' / '||cron FROM pgbx.status()")" "every 1 hour / 0 * * * *"
bad=$(P -d shop -c "SELECT pgbx.set_schedule('sometimes')" 2>&1); case "$bad" in *"can't read schedule"*) echo "  PASS bad schedule rejected: ${bad##*ERROR:  }" | cut -c1-140; pass=$((pass+1));; *) echo "  FAIL bad schedule accepted: $bad"; fail=$((fail+1));; esac
bad=$(P -d shop -c "SELECT pgbx.set_retention(max_days => 365)" 2>&1); case "$bad" in *"exceeds this server"*) echo "  PASS max_days above server limit rejected"; pass=$((pass+1));; *) echo "  FAIL limit not enforced: $bad"; fail=$((fail+1));; esac

echo "## 6. pause / resume"
echo "  $(P -d shop -c "SELECT pgbx.pause('migrating')")"
check "status paused" "$(P -d shop -c "SELECT state||'|'||paused_reason||'|'||coalesce(next_backup_at::text,'none') FROM pgbx.status()")" "paused|migrating|none"
P -d shop -c "SELECT pgbx.set_schedule('every 1 minute')" >/dev/null
before=$(P -d shop -c "SELECT count(*) FROM pgbx.history WHERE kind='backup' AND trigger='schedule'"); sleep 65
check "no automatic backup while paused" "$(P -d shop -c "SELECT count(*) FROM pgbx.history WHERE kind='backup' AND trigger='schedule'")" "$before"
echo "  $(P -d shop -c "SELECT pgbx.resume()")"
for _ in $(seq 75); do n=$(P -d shop -c "SELECT count(*) FROM pgbx.history WHERE kind='backup' AND trigger='schedule' AND state='done'"); [ "$n" -gt "$before" ] && break; sleep 1; done
check "scheduled backup after resume (every 1 minute)" "$([ "$n" -gt "$before" ] && echo yes || echo no)" yes
P -d shop -c "SELECT pgbx.set_schedule('daily at 02:00')" >/dev/null

echo "## 7. expiry: keep max 2 backups"
for _ in 1 2; do id=$(P -d shop -c "SELECT pgbx.backup_now()"); wait_job shop "$id" >/dev/null; sleep 1; done
total=$(P -d shop -c "SELECT count(*) FROM pgbx.backups"); echo "  backups before: $total"
echo "  $(P -d shop -c "SELECT pgbx.set_retention(max_backups => 2)")"
# wait for the expiry RESULT (S3 prune can take ~40s), not just the job state
for _ in $(seq 120); do [ "$(P -d shop -c "SELECT count(*) FROM pgbx.backups")" = 2 ] && break; sleep 1; done
for _ in $(seq 30); do [ "$(P -d shop -c "SELECT count(*) FROM pgbx.history WHERE kind='prune' AND state IN ('queued','running')")" = 0 ] && break; sleep 1; done
check "prune job finished (not stuck running)" "$(P -d shop -c "SELECT state FROM pgbx.history WHERE kind='prune' ORDER BY id DESC LIMIT 1")" done
check "only 2 backups kept" "$(P -d shop -c "SELECT count(*) FROM pgbx.backups")" 2
check "rest marked expired" "$(P -d shop -c "SELECT count(*) FROM pgbx.history WHERE state='expired'")" "$((total-2))"

echo "## 8. streaming: a database big enough for a multipart upload"
P -c "DROP DATABASE IF EXISTS big WITH (FORCE)" -c "DROP DATABASE IF EXISTS big_restored WITH (FORCE)" -c "CREATE DATABASE big"
P -d big -c "CREATE TABLE blob AS SELECT g AS id, md5(g::text) || md5((g*7)::text) AS a, md5((g*13)::text) AS b FROM generate_series(1,1000000) g"
for _ in $(seq 120); do st=$(P -d big -c "SELECT state FROM pgbx.history WHERE kind='backup' AND trigger='first'" 2>/dev/null); case "$st" in done|failed) break;; esac; sleep 2; done
id=$(P -d big -c "SELECT pgbx.backup_now()"); t=$SECONDS; r=$(wait_job big "$id"); [ "${r%% *}" = done ] || { sleep 60; r=$(wait_job big "$id"); }
bytes=$(echo "$r" | awk '{print $3}'); echo "  big backup: $r ($((SECONDS-t))s)"
check "big dump > 8 MiB (multipart)" "$([ "${bytes:-0}" -gt 8388608 ] && echo yes || echo "no ($bytes)")" yes
id=$(P -d big -c "SELECT pgbx.restore(into_db => 'big_restored')"); r=$(wait_job big "$id"); [ "${r%% *}" = done ] || { sleep 60; r=$(wait_job big "$id"); }
echo "  big restore: ${r%% *}"; check "1,000,000 rows streamed back" "$(P -d big_restored -c 'SELECT count(*) FROM blob')" 1000000

echo "## 9. restore test (verify)"
vs=$(P -d shop -c "SELECT pgbx.set_verify_schedule('weekly on sunday at 04:00')"); echo "  $vs"
check "set_verify_schedule()" "${vs%% (*}" 'restore test "weekly on sunday at 04:00"'
id=$(P -d shop -c "SELECT pgbx.verify_now()"); r=$(wait_job shop "$id"); echo "  verify #$id: ${r%% *}"
check "verify passed" "${r%% *}" done
check "status shows verify result" "$(P -d shop -c "SELECT left(last_verify_result, 3) FROM pgbx.status()")" "ok:"
check "scratch database dropped" "$(P -c "SELECT count(*) FROM pg_database WHERE datname LIKE 'pgbx_verify_%'")" 0
check "set_verify_schedule('never')" "$(P -d shop -c "SELECT pgbx.set_verify_schedule('never')")" "restore tests disabled"

echo "## 10. alerts on failure"
docker compose -f compose.test.yml exec -T -u postgres db sh -c ': > /var/lib/postgresql/alerts.log'
id=$(P -d shop -c "SELECT pgbx.restore(into_db => 'shop')"); r=$(wait_job shop "$id"); echo "  restore into existing db: ${r%% *} — ${r#* * * }" | cut -c1-120
check "failed as expected" "${r%% *}" failed
sleep 1; alertline=$(docker compose -f compose.test.yml exec -T db cat /var/lib/postgresql/alerts.log)
echo "  alert: $(echo "$alertline" | cut -c1-140)"
check "alert_command got the failure" "$(echo "$alertline" | grep -c '"kind":"restore"')" 1

echo "## 11. doctor(): per-database checks only"
check "doctor() runs only in the admin db" "$(P -d shop -c "SELECT count(*) FROM pgbx.doctor()" 2>&1 | grep -c 'run in database')" 1
check "doctor(): s3 settings ok" "$(P -c "SELECT ok FROM pgbx.doctor() WHERE name='s3 settings'")" t
check "doctor(): worker alive" "$(P -c "SELECT ok FROM pgbx.doctor() WHERE name='workers'")" t
check "doctor(): archive_mode row is info only" "$(P -c "SELECT ok FROM pgbx.doctor() WHERE name='archive_mode'")" t

echo "## 12. access control"
P -c "DROP ROLE IF EXISTS app_user" -c "DROP ROLE IF EXISTS viewer_user" -c "DROP ROLE IF EXISTS admin_user"
P -c "CREATE ROLE app_user LOGIN" -c "CREATE ROLE viewer_user LOGIN IN ROLE pgbx_viewer" -c "CREATE ROLE admin_user LOGIN IN ROLE pgbx_admin"
AS() { u=$1; shift; docker compose -f compose.test.yml exec -T db psql -U "$u" -qAt "$@" 2>&1; }
denied() { case "$1" in *"permission denied"*) echo denied;; *) echo "allowed: $(echo "$1" | head -1 | cut -c1-60)";; esac; }
check "plain user cannot read status"      "$(denied "$(AS app_user -d shop -c 'SELECT state FROM pgbx.status()')")" denied
check "plain user cannot pause"            "$(denied "$(AS app_user -d shop -c "SELECT pgbx.pause('x')")")" denied
check "plain user cannot get a download link" "$(denied "$(AS app_user -d shop -c 'SELECT pgbx.download_url()')")" denied
check "viewer can read status"             "$(AS viewer_user -d shop -c 'SELECT state IS NOT NULL FROM pgbx.status()')" t
check "viewer can list backups"            "$(AS viewer_user -d shop -c 'SELECT count(*) > 0 FROM pgbx.backups')" t
check "viewer cannot pause"                "$(denied "$(AS viewer_user -d shop -c "SELECT pgbx.pause('x')")")" denied
check "viewer cannot write tables directly" "$(denied "$(AS viewer_user -d shop -c "UPDATE pgbx.config SET enabled=false")")" denied
check "admin can pause"                    "$(AS admin_user -d shop -c "SELECT left(pgbx.pause('admin test'), 24)")" "automatic backups paused"
check "admin can resume"                   "$(AS admin_user -d shop -c "SELECT left(pgbx.resume(), 26)")" "automatic backups resumed;"
check "admin can set schedule"             "$(AS admin_user -d shop -c "SELECT left(pgbx.set_schedule('daily at 02:00'), 31)")" 'schedule set to "daily at 02:00'
check "admin cannot write tables directly" "$(denied "$(AS admin_user -d shop -c "DELETE FROM pgbx.history")")" denied
check "admin cannot call the internal signer" "$(denied "$(AS admin_user -d shop -c "SELECT pgbx._presign('x', 600)")")" denied

echo "## 13. download_url (presigned, no keys needed)"
P -d shop -c "DROP TABLE IF EXISTS urltest; CREATE TABLE urltest AS SELECT g AS n FROM generate_series(1,10) g"
id=$(P -d shop -c "SELECT pgbx.backup_now()"); wait_job shop "$id" >/dev/null
url=$(AS admin_user -d shop -c "SELECT pgbx.download_url(expires => '10 minutes')")
case "$url" in http*X-Amz-Signature*) echo "  PASS admin got a signed link (${url%%\?*}?…)"; pass=$((pass+1));; *) echo "  FAIL no link: $url"; fail=$((fail+1));; esac
check "link downloads (HTTP 200)" "$(curl -s -o /dev/null -w '%{http_code}' "$url")" 200
P -c "DROP DATABASE IF EXISTS shop_from_url WITH (FORCE)" -c "CREATE DATABASE shop_from_url TEMPLATE template0"
curl -s "$url" | docker compose -f compose.test.yml exec -T db pg_restore -U postgres --no-owner -d shop_from_url 2>/dev/null
check "curl | pg_restore from the link" "$(P -d shop_from_url -c 'SELECT count(*) FROM urltest')" 10
check "expiry limit enforced" "$(P -d shop -c "SELECT pgbx.download_url(expires => '30 seconds')" 2>&1 | grep -c 'between 1 minute and 7 days')" 1
check "link request is audited" "$(P -d shop -c "SELECT count(*) > 0 FROM pgbx.history WHERE params ? 'download_url'")" t

echo "## 14. overview() — every database in one place"
sleep 8
ov=$(P -c "SELECT count(*) FROM pgbx.overview() WHERE database IN ('shop','big','postgres')")
check "overview lists the databases" "$ov" 3
check "viewer can read overview" "$(AS viewer_user -d postgres -c "SELECT count(*) > 0 FROM pgbx.overview()")" t
P -c "SELECT database, state, schedule, last_backup_size, coalesce(last_error,'') FROM pgbx.overview()" | sed 's/^/  /'

echo "## 15. data scope: every table always, rows only where wanted"
P -c "DROP DATABASE IF EXISTS scoped WITH (FORCE)" -c "CREATE DATABASE scoped"
for r in r1 r2 r3; do P -c "DROP DATABASE IF EXISTS scoped_$r WITH (FORCE)"; done
P -d scoped -c "CREATE TABLE users AS SELECT g id FROM generate_series(1,100) g;
                CREATE TABLE sessions AS SELECT g id FROM generate_series(1,500) g;
                CREATE TABLE audit_log_2026 AS SELECT g id FROM generate_series(1,50) g;
                CREATE SCHEMA billing; CREATE TABLE billing.invoices AS SELECT g id FROM generate_series(1,30) g;"
for _ in $(seq 60); do P -d scoped -c "SELECT 1 FROM pgbx.config" >/dev/null 2>&1 && break; sleep 1; done
counts() { P -d "$1" -c "SELECT (SELECT count(*) FROM users)||'/'||(SELECT count(*) FROM sessions)||'/'||(SELECT count(*) FROM audit_log_2026)||'/'||(SELECT count(*) FROM billing.invoices)"; }
scoped_round() { # name -> backup, restore into scoped_<name>, print users/sessions/audit/invoices
  local id; id=$(P -d scoped -c "SELECT pgbx.backup_now()"); wait_job scoped "$id" >/dev/null
  id=$(P -d scoped -c "SELECT pgbx.restore(into_db => 'scoped_$1')"); wait_job scoped "$id" >/dev/null
  counts "scoped_$1"; }
echo "  $(P -d scoped -c "SELECT pgbx.set_data_scope(exclude => ARRAY['public.sessions', 'audit_log_*'])")"
check "rowless_tables() after exclude" "$(P -d scoped -c "SELECT string_agg(table_name, ',') FROM pgbx.rowless_tables()")" "public.audit_log_2026,public.sessions"
check "exclude: tables kept, those rows skipped (users/sessions/audit/invoices)" "$(scoped_round r1)" "100/0/0/30"
echo "  $(P -d scoped -c "SELECT pgbx.set_data_scope(include => ARRAY['billing.*', 'users'])")"
check "include: only billing.* and users keep rows" "$(scoped_round r2)" "100/0/0/30"
check "status shows the scope" "$(P -d scoped -c "SELECT data_scope FROM pgbx.status()")" "all tables; rows of only billing.*, users (2 tables backed up without rows)"
check "backup records skipped tables" "$(P -d scoped -c "SELECT params->'rows_skipped' FROM pgbx.history WHERE kind='backup' AND state='done' ORDER BY id DESC LIMIT 1")" '["public.audit_log_2026", "public.sessions"]'
id=$(P -d scoped -c "SELECT pgbx.verify_now()"); r=$(wait_job scoped "$id"); check "restore test passes on a scoped backup" "${r%% *}" done
echo "  $(P -d scoped -c "SELECT pgbx.set_data_scope()")"
check "reset: every row is back" "$(scoped_round r3)" "100/500/50/30"
check "admin may set the scope" "$(AS admin_user -d scoped -c "SELECT left(pgbx.set_data_scope(), 22)")" "backups keep every tab"

echo "## status()"; P -d shop -x -c "SELECT * FROM pgbx.status()" | sed 's/^/  /'
echo "## backups"; P -d shop -c "SELECT id, taken_at::timestamp(0), trigger, size, s3_key FROM pgbx.backups" | sed 's/^/  /'
echo "## history of shop"; P -d shop -c "SELECT id, kind, trigger, state, coalesce(s3_key,'-') FROM pgbx.history ORDER BY id" | sed 's/^/  /'
echo "RESULT: $pass passed, $fail failed"; [ $fail -eq 0 ]
