#!/bin/sh
# Throwaway smoke test for `pgbx ui` + audit retention (runs INSIDE the pgbx-dev image, no S3 needed):
#   docker run --rm --name pgbx-ui-smoke -v "$PWD":/src -v pgbx-ui-target:/src/target \
#     -v pgbx-ui-clitarget:/src/cli/target -w /src pgbx-dev sh tests/ui_smoke.sh
set -u
cargo pgrx package --pg-config /usr/lib/postgresql/16/bin/pg_config >/dev/null 2>&1 || { echo "extension build failed"; exit 1; }
cp target/release/pgbx-pg16/usr/lib/postgresql/16/lib/pgbx.so /usr/lib/postgresql/16/lib/
cp target/release/pgbx-pg16/usr/share/postgresql/16/extension/* /usr/share/postgresql/16/extension/
(cd cli && cargo build --release -q) && cp cli/target/release/pgbx /usr/local/bin/pgbx || exit 1
command -v curl >/dev/null || { apt-get update -qq >/dev/null && apt-get install -y -qq curl >/dev/null; }
B=/usr/lib/postgresql/16/bin; D=/tmp/pgd; W=/tmp/pgbxwd
mkdir -p $W /var/run/postgresql && chown postgres $W /var/run/postgresql
su postgres -c "$B/initdb -D $D -A trust >/dev/null"
cat >> $D/postgresql.conf <<CONF
shared_preload_libraries = 'pgbx'
pgbx.poll_seconds = 2
pgbx.audit_days = 30
CONF
su postgres -c "$B/pg_ctl -D $D -l $W/pg.log -w start >/dev/null"
P() { su postgres -c "psql -XAtq -d ${2:-postgres} -c \"$1\""; }
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "  PASS $1"; pass=$((pass+1)); else echo "  FAIL $1 (got '$2', want '$3')"; fail=$((fail+1)); fi; }
for _ in $(seq 30); do [ "$(P "SELECT count(*) FROM pgbx.server_overview" 2>/dev/null)" -ge 1 ] 2>/dev/null && break; sleep 1; done
P "CREATE DATABASE shop" >/dev/null; sleep 5

echo "--- schema 0.5.0"
check "extension version" "$(P "SELECT extversion FROM pg_extension WHERE extname='pgbx'")" "0.5.0"
check "history.who exists" "$(P "SELECT count(*) FROM information_schema.columns WHERE table_name='history' AND column_name='who'")" "1"
check "audit_days GUC" "$(P "SHOW pgbx.audit_days")" "30"

echo "--- audit retention (old rows, restart => worker prunes on its first pass)"
P "INSERT INTO pgbx.history (kind, trigger, state, requested_at, finished, s3_key) VALUES
   ('backup','schedule','done', now()-interval '40 days', now()-interval '40 days', 'k/kept.dump'),
   ('backup','schedule','expired', now()-interval '40 days', now()-interval '40 days', 'k/gone.dump'),
   ('backup','schedule','failed', now()-interval '40 days', now()-interval '40 days', NULL),
   ('restore','manual','queued', now()-interval '50 days', NULL, NULL),
   ('verify','schedule','done', now()-interval '45 days', now()-interval '45 days', NULL),
   ('config','migration','done', now()-interval '35 days', now()-interval '35 days', NULL)" shop
P "INSERT INTO pgbx.history (kind, trigger, state, requested_at, finished) VALUES ('config','migration','done', now(), now())" shop
su postgres -c "$B/pg_ctl -D $D -l $W/pg.log -w restart >/dev/null"; sleep 8
check "kept backup survives" "$(P "SELECT count(*) FROM pgbx.history WHERE s3_key='k/kept.dump'" shop)" "1"
check "expired backup pruned" "$(P "SELECT count(*) FROM pgbx.history WHERE s3_key='k/gone.dump'" shop)" "0"
check "old config pruned" "$(P "SELECT count(*) FROM pgbx.history WHERE kind='config' AND requested_at < now()-interval '30 days'" shop)" "0"
check "old queued job survives" "$(P "SELECT count(*) FROM pgbx.history WHERE kind='restore'" shop)" "1"
check "latest verify survives" "$(P "SELECT count(*) FROM pgbx.history WHERE kind='verify'" shop)" "1"

echo "--- viewer login role"
P "CREATE ROLE ui_viewer LOGIN IN ROLE pgbx_viewer" >/dev/null
P "SELECT pgbx.download_url()" shop >/dev/null 2>&1   # an audit row (fails without S3, still fine)
su postgres -c "echo \$\$ > $W/ui.pid; exec pgbx ui --user ui_viewer --listen 127.0.0.1:8432 --strict --json --host /var/run/postgresql" > $W/ui.out 2> $W/ui.err &
sleep 2
check "strict viewer starts" "$(head -1 $W/ui.out | python3 -c 'import json,sys;d=json.load(sys.stdin);print(d["ok"],d["warnings"])')" "True []"
U=http://127.0.0.1:8432
code() { curl -s -o /dev/null -w '%{http_code}' "$@"; }
check "GET / 200" "$(code $U/)" "200"
check "page has no external refs" "$(curl -s $U/ | grep -c 'https\?://')" "0"
for e in overview "db/shop" "timeline?days=30" health; do check "GET /api/$e 200" "$(code "$U/api/$e")" "200"; done
J() { curl -s "$U/api/$1" | python3 -c "import json,sys;d=json.load(sys.stdin);print($2)"; }
check "overview shape" "$(J overview 'd["ok"], d["connection"]["read_only"], isinstance(d["databases"],list) and len(d["databases"])>=2')" "True on True"
check "db shape" "$(J db/shop 'd["status"]["database"], isinstance(d["backups"],list), isinstance(d["history"],list)')" "shop True True"
check "timeline shape" "$(J 'timeline?days=30' 'd["days"], all(k in d["rows"][0] for k in ("database","kind","state","who","at")), "shop" in d["databases"]')" "30 True True"
check "timeline newest first" "$(J 'timeline?days=30' 'all(a["at"]>=b["at"] for a,b in zip(d["rows"],d["rows"][1:]))')" "True"
check "no whole-server endpoint" "$(code $U/api/cluster)" "404"
check "health shape" "$(J health 'isinstance(d["checks"],list) and len(d["checks"])>3 and "fix" in d["checks"][0]')" "True"
check "who recorded" "$(P "SELECT who FROM pgbx.history WHERE kind='config' ORDER BY id DESC LIMIT 1" shop)" "postgres"
check "unknown db 502" "$(code $U/api/db/nope)" "502"
check "unknown path 404" "$(code $U/api/nope)" "404"
for m in POST PUT DELETE PATCH; do check "$m -> 405" "$(code -X $m $U/api/overview)" "405"; done
check "POST / -> 405" "$(code -X POST -d x=1 $U/)" "405"
check "foreign Host -> 403" "$(code -H 'Host: evil.example' $U/api/overview)" "403"
check "UI sessions are read-only" "$(P "SELECT count(*) FROM pg_stat_activity WHERE application_name='pgbx' AND usename='ui_viewer' AND state='active' AND query ILIKE '%insert%'")" "0"
kill $(cat $W/ui.pid) 2>/dev/null; sleep 1

echo "--- write through a UI-style connection fails"
out=$(su postgres -c "psql -XAtq -d shop -c 'SET default_transaction_read_only = on' -c 'SELECT pgbx.backup_now()'" 2>&1)
case "$out" in *"read-only transaction"*) r=refused;; *) r="$out";; esac
check "backup_now under read-only" "$r" "refused"
out=$(su postgres -c "psql -XAtq -d shop -c 'SET default_transaction_read_only = on' -c 'SELECT pgbx.pause()'" 2>&1)
case "$out" in *"read-only transaction"*) r=refused;; *) r="$out";; esac
check "pause under read-only" "$r" "refused"

echo "--- superuser: warned, --strict refuses"
su postgres -c "pgbx ui --listen 127.0.0.1:8433 --strict --json" > $W/su.out 2>&1
check "strict superuser refused" "$(python3 -c "import json;d=json.load(open('$W/su.out'));print(d['ok'], 'SUPERUSER' in d['error'])")" "False True"
su postgres -c "echo \$\$ > $W/ui.pid; exec pgbx ui --listen 0.0.0.0:8434" > $W/su2.out 2> $W/su2.err &
sleep 2
check "superuser warned" "$(grep -c 'WARNING: role' $W/su2.err)" "1"
check "non-loopback warned" "$(grep -c 'your responsibility; it is read-only' $W/su2.err)" "1"
check "superuser UI still serves" "$(code -H 'Host: x' http://127.0.0.1:8434/api/health)" "200"
kill $(cat $W/ui.pid) 2>/dev/null; sleep 1

su postgres -c "$B/pg_ctl -D $D -m fast stop >/dev/null"
echo "== ui smoke: pass=$pass fail=$fail"
[ $fail -eq 0 ]
