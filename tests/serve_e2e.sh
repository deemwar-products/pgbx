#!/usr/bin/env bash
# End-to-end check of `pgbx serve` (the local web app): the binary built on THIS machine talks to
#   1. the docker test stack (docker/compose.test.yml, needs the server up; run after tests/e2e.sh so there is history):
#      app + API endpoints, no token -> 401, foreign Host -> 403, actions refused without --allow-safe, the safe ones
#      allowed with it (backup now, restore into a NEW database, cancel a queued job), the query guard, Save to memory;
#   2. a STOCK postgres (no pgbx extension, like tests/client_only_e2e.sh): the overview says backups are off.
# Run: tests/serve_e2e.sh     (needs docker, curl, jq; bash 3.2-safe; exits non-zero on any failure)
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
B=${PGBX_BIN:-$ROOT/cli/target/debug/pgbx}
[ -n "${PGBX_BIN:-}" ] || cargo build -q --manifest-path "$ROOT/cli/Cargo.toml" || exit 1   # always the current source
cd "$ROOT/docker"
DC="docker compose -f compose.test.yml"
X() { $DC exec -T -u postgres db "$@"; }
P() { X psql -v ON_ERROR_STOP=1 -qAt "$@"; }
TPORT=${SERVE_TEST_PG_PORT:-55439}; CO=pgbx-serve-co-pg; COPORT=${SERVE_CO_PG_PORT:-55497}
W=$(mktemp -d)
export PGBX_CONFIG_DIR=$W/config PGBX_STATE_DIR=$W/state PGBX_MEMORY_DIR=$W/mem
PIDS=""
cleanup() { for p in $PIDS; do kill "$p" 2>/dev/null; done; docker rm -f $CO >/dev/null 2>&1; rm -rf "$W"; }
trap cleanup EXIT
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "  PASS $1"; pass=$((pass+1)); else echo "  FAIL $1 (got '$2', want '$3')"; fail=$((fail+1)); fi; }

# start NAME ARGS...: run pgbx serve in the background; sets URL (http://127.0.0.1:PORT) and TOK
start() {
  local name=$1; shift
  "$B" serve --no-open --json "$@" > "$W/$name.out" 2> "$W/$name.err" &
  PIDS="$PIDS $!"
  for _ in $(seq 100); do [ -s "$W/$name.out" ] && break; sleep 0.2; done
  URL=$(jq -r '.url' < "$W/$name.out" | sed 's|/#token=.*||'); TOK=$(jq -r '.url' < "$W/$name.out" | sed 's|.*#token=||')
}
code() { curl -s -o /dev/null -w '%{http_code}' "$@"; }                       # code ARGS... -> HTTP status
G() { curl -s -H "X-Pgbx-Token: $TOK" "$URL$1"; }                           # GET with the token
PJ() { curl -s -H "X-Pgbx-Token: $TOK" -H 'Content-Type: application/json' -d "$2" "$URL$1"; }   # POST JSON

P -c "SELECT 1" >/dev/null 2>&1 || { echo "test stack not up (docker compose -f docker/compose.test.yml up -d; tests/e2e.sh)"; exit 1; }
export PGPASSWORD=test-only-not-secret
CONN="--host 127.0.0.1 --port $TPORT --user postgres"

echo "## 1. read-only (the default)"
start ro $CONN
check "starts: json line, read-only, loopback, random port" "$(jq -r '"\(.ok) \(.safety) \(.listen|startswith("127.0.0.1:")) \(.listen|endswith(":8432"))"' < "$W/ro.out")" "true read-only true false"
check "token: 32 hex chars, in the URL fragment" "$(printf %s "$TOK" | grep -cE '^[0-9a-f]{32}$')" 1
check "GET / -> the app" "$(code "$URL/")" 200
check "app loads its embedded script and style" "$(curl -s "$URL/" | grep -c '/assets/index.js\|/assets/index.css')" 2
check "GET /assets/index.js" "$(code "$URL/assets/index.js")" 200
check "app fetches no other origin" "$(curl -s "$URL/assets/index.js" | grep -cE 'fetch\(["'\'']https?:|sendBeacon')" 0
check "CSP: connect-src 'self'" "$(curl -sI "$URL/" | grep -ci "connect-src 'self'")" 1
check "no token -> 401" "$(code "$URL/api/overview")" 401
check "wrong token -> 401" "$(code -H 'X-Pgbx-Token: 0123456789abcdef0123456789abcdef' "$URL/api/overview")" 401
check "foreign Host -> 403" "$(code -H 'Host: evil.example' -H "X-Pgbx-Token: $TOK" "$URL/api/overview")" 403
check "foreign Host on the app -> 403" "$(code -H 'Host: evil.example' "$URL/")" 403
check "PUT -> 405" "$(code -X PUT -H "X-Pgbx-Token: $TOK" "$URL/api/overview")" 405
check "unknown api -> 404" "$(code -H "X-Pgbx-Token: $TOK" "$URL/api/nope")" 404
check "session: read-only, no actions" "$(G /api/session | jq -r '"\(.ok) \(.allow_safe) \(.actions|length)"')" "true false 0"
for e in /api/session /api/overview /api/queue /api/load /api/health /api/db/postgres /api/suggest/postgres /api/memory/postgres; do
  check "GET $e" "$(code -H "X-Pgbx-Token: $TOK" "$URL$e")" 200
done
ov=$(G /api/overview)
check "overview: backups on, databases, read-only session" "$(echo "$ov" | jq -r '"\(.backups) \(.databases|length>0) \(.connection.read_only)"')" "on true on"
check "overview: per database state, last backup, next run, kept, restore test" \
  "$(echo "$ov" | jq -r '.databases[0] | has("state") and has("last_backup_at") and has("next_backup_at") and has("backups_kept") and has("last_verify") and has("last_error")')" true
check "overview: quiet-window data" "$(echo "$ov" | jq -r '.windows|type')" array
check "queue: jobs + slots" "$(G /api/queue | jq -r '"\(.jobs|type) \(.slots|has("max_concurrent_jobs"))"')" "array true"
check "load: sample + settings + databases" "$(G /api/load | jq -r '"\(.sample|type) \(.settings|has("load_gate")) \(.databases|type)"')" "object true array"
h=$(G /api/health)
check "health: doctor rows with fixes" "$(echo "$h" | jq -r '(.checks|length>3) and (.checks[0]|has("fix")) and has("healthy")')" true
check "health: advice rows are warnings" "$(echo "$h" | jq -r '[.checks[]|select(.name=="schedule_in_quiet_window" or .name=="eta_accuracy")|.warning]|all')" true
d=$(G /api/db/postgres)
check "db detail: status, backups, history" "$(echo "$d" | jq -r '"\(.status.schedule != null) \(.backups|type) \(.history|length>0)"')" "true array true"
check "suggestion: the configure() call + CLI to copy" "$(G /api/suggest/postgres | jq -r '"\(.ok) \(.cli)"')" "true pgbx schedule suggest --db postgres --apply"
check "no such database -> error" "$(G /api/db/no_such_db_x | jq -r '.error')" "no such database 'no_such_db_x'"
q=$(PJ /api/query '{"db":"postgres","sql":"SELECT 1 AS n, $$a,b$$ AS s"}')
check "query: rows + columns" "$(echo "$q" | jq -r '"\(.rows[0].n) \(.rows[0].s) \(.columns[0].type) \(.read_only_transaction)"')" "1 a,b int4 true"
check "query guard refuses a write" "$(PJ /api/query '{"db":"postgres","sql":"DELETE FROM pgbx.history"}' | jq -r '"\(.ok) \(.error|test("SELECT-style statements only"))"')" "false true"
check "query guard refuses two statements" "$(PJ /api/query '{"db":"postgres","sql":"SELECT 1; SELECT 2"}' | jq -r '.error|test("exactly one")')" true
check "query guard refuses backup_now()" "$(PJ /api/query '{"db":"postgres","sql":"SELECT pgbx.backup_now()"}' | jq -r '.error|test("side effects")')" true
check "query: max_rows caps" "$(PJ /api/query '{"db":"postgres","sql":"SELECT g FROM generate_series(1,50) g","max_rows":5}' | jq -r '"\(.row_count) \(.truncated)"')" "5 true"
check "memory: none yet" "$(G /api/memory/postgres | jq -r '"\(.enabled) \(.exists) \(.questions|length)"')" "true false 0"
r=$(PJ /api/memory/postgres '{"name":"one","note":"just a one","sql":"SELECT 1"}')
check "Save to memory appends (MEM-W-1 shape)" "$(echo "$r" | jq -r .ok)|$(cat "$W/mem/127.0.0.1/postgres/memories.md" 2>/dev/null | tr '\n' '~')" 'true|~## one~just a one~```sql~SELECT 1~```~'
check "memory: the saved question is read back" "$(G /api/memory/postgres | jq -r '.questions[0] | "\(.name)|\(.sql)"')" "one|SELECT 1"
check "memory: bad name refused" "$(PJ /api/memory/postgres '{"name":"","sql":"SELECT 1"}' | jq -r .ok)" false
for a in backup verify restore cancel; do
  check "POST /api/action/$a refused without --allow-safe" "$(code -H "X-Pgbx-Token: $TOK" -H 'Content-Type: application/json' -d '{"db":"postgres"}' "$URL/api/action/$a")" 403
done
check "guarded action does not exist" "$(code -H "X-Pgbx-Token: $TOK" -H 'Content-Type: application/json' -d '{"db":"postgres"}' "$URL/api/action/pause")" 404
check "nothing was queued by the refusals" "$(P -c "SELECT count(*) FROM pgbx.history WHERE requested_at > now() - interval '30 seconds' AND trigger = 'manual'")" 0

echo "## 2. --allow-safe"
P -c "DROP DATABASE IF EXISTS serve_e2e_restored" >/dev/null 2>&1
start safe $CONN --allow-safe
check "starts: safety safe" "$(jq -r .safety < "$W/safe.out")" safe
check "session: the four safe actions" "$(G /api/session | jq -c .actions)" '["backup","verify","restore","cancel"]'
r=$(PJ /api/action/backup '{"db":"postgres"}')
check "backup now: queued, through the CLI path" "$(echo "$r" | jq -r '"\(.ok) \(.action) \(.job_id|type) \(.cli)"')" "true backup number pgbx now --db postgres"
check "restore into the source refused" "$(PJ /api/action/restore '{"db":"postgres","into":"postgres"}' | jq -r '.error|test("NEW database")')" true
check "restore into an existing database refused" "$(PJ /api/action/restore '{"db":"postgres","into":"template1"}' | jq -r '.error|test("already exists")')" true
check "restore --time needs a UTC offset" "$(PJ /api/action/restore '{"db":"postgres","into":"x_new","time":"2026-01-01 10:00"}' | jq -r '.error|test("UTC offset")')" true
# a queued job the worker leaves alone (deferred for an hour): the UI cancels it
jid=$(P -c "INSERT INTO pgbx.history (kind, trigger, params) VALUES ('backup', 'manual', jsonb_build_object('deferred_until', now() + interval '1 hour', 'defer_reason', 'serve_e2e')) RETURNING id" | head -1)
r=$(PJ /api/action/cancel "{\"db\":\"postgres\",\"job_id\":$jid}")
check "cancel a queued job" "$(echo "$r" | jq -r '"\(.ok) \(.message|test("cancelled before it started"))"')" "true true"
check "history shows it cancelled" "$(G /api/db/postgres | jq -r --argjson id "$jid" '.history[]|select(.id==$id)|.state')" cancelled
r=$(PJ /api/action/cancel "{\"db\":\"postgres\",\"job_id\":$jid}")
check "cancel of a job that is not queued refused (UI never stops a running job)" "$(echo "$r" | jq -r '"\(.ok) \(.error|test("only cancels queued"))"')" "false true"
src=$(P -c "SELECT database FROM pgbx.server_overview WHERE backups_kept > 0 AND database NOT LIKE 'serve_e2e%' ORDER BY last_backup_at DESC LIMIT 1")
if [ -n "$src" ]; then
  r=$(PJ /api/action/restore "{\"db\":\"$src\",\"into\":\"serve_e2e_restored\"}")
  rid=$(echo "$r" | jq -r .job_id)
  check "restore into a NEW database: queued" "$(echo "$r" | jq -r '"\(.ok) \(.action)"')" "true restore"
  st=""; for _ in $(seq 150); do st=$(P -d "$src" -c "SELECT state FROM pgbx.history WHERE id = $rid"); case $st in done|failed|cancelled) break;; esac; sleep 2; done
  check "restore finished, new database exists" "$st|$(P -c "SELECT count(*) FROM pg_database WHERE datname = 'serve_e2e_restored'")" "done|1"
  P -c "DROP DATABASE IF EXISTS serve_e2e_restored" >/dev/null
else
  echo "  SKIP restore into a NEW database (no database has a backup yet: run tests/e2e.sh first)"
fi

echo "## 3. client-only server (stock postgres, no pgbx extension)"
docker rm -f $CO >/dev/null 2>&1
docker run -d --name $CO -p "127.0.0.1:$COPORT:5432" -e POSTGRES_HOST_AUTH_METHOD=trust postgres:16-alpine >/dev/null
for _ in $(seq 60); do docker logs $CO 2>&1 | grep -q "init process complete" && docker exec $CO pg_isready -q -U postgres 2>/dev/null && break; sleep 1; done
for _ in $(seq 30); do docker exec $CO psql -qAtU postgres -c "SELECT 1" >/dev/null 2>&1 && break; sleep 1; done
docker exec $CO psql -qU postgres -c "CREATE DATABASE app" >/dev/null
unset PGPASSWORD
start co --host 127.0.0.1 --port $COPORT --user postgres
OFF="pgbx extension not installed on this server: backups are off; queries, profiles and tunnels work"
ov=$(G /api/overview)
check "overview: backups off, said plainly" "$(echo "$ov" | jq -r '"\(.ok) \(.backups) \(.info)"')" "true off $OFF"
check "overview: the databases are still listed" "$(echo "$ov" | jq -r '[.databases[].database]|index("app") != null')" true
check "overview: how to turn backups on" "$(echo "$ov" | jq -r '.next_steps[0]|test("pgbx setup server")')" true
check "health: healthy, extension row is info" "$(G /api/health | jq -r '"\(.healthy) \(.checks[]|select(.name=="backups (pgbx extension)")|.ok)"')" "true true"
check "query works without the extension" "$(PJ /api/query '{"db":"app","sql":"SELECT 41 + 1 AS n"}' | jq -r '.rows[0].n')" 42
check "queue: refused in one plain sentence" "$(G /api/queue | jq -r '"\(.ok) \(.error|startswith("pgbx extension not installed"))"')" "false true"

echo "== serve_e2e: pass=$pass fail=$fail"
[ $fail -eq 0 ]
