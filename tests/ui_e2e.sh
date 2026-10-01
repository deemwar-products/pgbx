#!/usr/bin/env bash
# End-to-end check of `pgbx ui` (read-only audit UI) against docker/compose.test.yml (needs the server up; run after
# tests/e2e.sh so there is real backup / whole-server history). bash 3.2-safe; exits non-zero on any failure.
# The UI runs INSIDE the db container on 127.0.0.1:8432 as a pgbx_viewer login; requests go over bash /dev/tcp.
set -u
cd "$(dirname "$0")/../docker"
DC="docker compose -f compose.test.yml"
X() { $DC exec -T -u postgres db "$@"; }
P() { X psql -v ON_ERROR_STOP=1 -qAt "$@"; }
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "  PASS $1"; pass=$((pass+1)); else echo "  FAIL $1 (got '$2', want '$3')"; fail=$((fail+1)); fi; }
# req METHOD PATH [HOST] -> full HTTP response
req() { X bash -c "exec 3<>/dev/tcp/127.0.0.1/8432; printf '%s %s HTTP/1.1\r\nHost: ${3:-127.0.0.1:8432}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n' '$1' '$2' >&3; cat <&3"; }
status() { req "$@" | head -1 | awk '{print $2}'; }
body() { req GET "$1" | sed '1,/^\r$/d'; }

P -c "SELECT 1" >/dev/null || { echo "server not up"; exit 1; }
echo "== pgbx ui"
P -c "DROP ROLE IF EXISTS pgbx_ui_e2e" -c "CREATE ROLE pgbx_ui_e2e LOGIN IN ROLE pgbx_viewer" >/dev/null
stop_ui() { X sh -c '[ -f /tmp/pgbx-ui.pid ] && kill $(cat /tmp/pgbx-ui.pid) 2>/dev/null; rm -f /tmp/pgbx-ui.pid'; }
stop_ui
$DC exec -T -d -u postgres db sh -c "echo \$\$ > /tmp/pgbx-ui.pid; exec pgbx ui --user pgbx_ui_e2e --strict --json > /tmp/pgbx-ui.out 2> /tmp/pgbx-ui.err"
sleep 2
check "viewer starts under --strict" "$(X cat /tmp/pgbx-ui.out | jq -r '"\(.ok) \(.warnings|length) \(.safety)"')" "true 0 read-only"

check "GET /" "$(status GET /)" "200"
check "page has no external fetches" "$(body / | grep -c 'https\?://')" "0"
for e in /api/overview /api/db/postgres "/api/timeline?days=30" /api/health; do check "GET $e" "$(status GET "$e")" "200"; done
check "overview: databases, read-only" "$(body /api/overview | jq -r '"\(.ok) \(.connection.read_only) \(.databases|length>0)"')" "true on true"
check "timeline: rows with who/when/what" "$(body '/api/timeline?days=30' | jq -r '(.rows|length>0) and (.rows[0]|has("who") and has("at") and has("kind") and has("database"))')" "true"
check "timeline: has backups" "$(body '/api/timeline?days=30' | jq -r '[.rows[]|select(.kind=="backup")]|length>0')" "true"
check "timeline: newest first" "$(body '/api/timeline?days=30' | jq -r '[.rows[].at] as $a | $a == ($a|sort|reverse)')" "true"
check "whole-server endpoint is gone (404)" "$(status GET /api/cluster)" "404"
check "page has no whole-server screen" "$(body / | grep -c 'Whole server')" "0"
check "health: doctor rows" "$(body /api/health | jq -r '(.checks|length>3) and (.checks[0]|has("fix"))')" "true"
for m in POST PUT DELETE PATCH HEAD; do check "$m -> 405" "$(status $m /api/overview)" "405"; done
check "foreign Host -> 403" "$(status GET /api/overview evil.example)" "403"
check "unknown -> 404" "$(status GET /api/nope)" "404"
check "viewer cannot backup_now" "$(X psql -qAt -U pgbx_ui_e2e -d postgres -c 'SELECT pgbx.backup_now()' 2>&1 | grep -c 'permission denied')" "1"
check "read-only session refuses writes" "$(X psql -qAt -d postgres -c 'SET default_transaction_read_only = on' -c 'SELECT pgbx.backup_now()' 2>&1 | grep -c 'read-only transaction')" "1"
stop_ui

check "superuser refused under --strict" "$(X pgbx ui --listen 127.0.0.1:8435 --strict --json 2>/dev/null | jq -r '"\(.ok) \(.error|test("SUPERUSER"))"')" "false true"
P -c "DROP ROLE pgbx_ui_e2e" >/dev/null
echo "== ui_e2e: pass=$pass fail=$fail"
[ $fail -eq 0 ]
