#!/usr/bin/env bash
# End-to-end check of the pgbx CLI against docker/compose.test.yml (run after tests/e2e.sh; needs the server up).
# Run: tests/cli_e2e.sh     (bash 3.2-safe; exits non-zero on any failure)
#
# Step j restores from S3 onto a second, plain Postgres container (pgbx-cli-plain: no extension installed) on the
# compose network, the way a brand-new server would be rebuilt after losing the old one.
set -u
cd "$(dirname "$0")/../docker"
DC="docker compose -f compose.test.yml"
X() { $DC exec -T -u postgres db "$@"; }
P() { X psql -v ON_ERROR_STOP=1 -qAt "$@"; }
J() { X pgbx "$@" --json 2>/dev/null; }          # stdout only: must be exactly one JSON object
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "  PASS $1"; pass=$((pass+1)); else echo "  FAIL $1 (got '$2', want '$3')"; fail=$((fail+1)); fi; }
jq1() { echo "$1" | jq -r "$2" 2>/dev/null; }
# one JSON object on stdout with ok/command/safety
onejson() { [ "$(echo "$1" | jq -s 'length' 2>/dev/null)" = 1 ] && [ "$(jq1 "$1" 'has("ok")')" = true ] && echo yes || echo "no: $(echo "$1" | head -c 200)"; }
# refusal: one JSON object, ok:false, given exit code, error contains the text
refused() { # name out rc want_rc needle
  local e; e=$(jq1 "$2" '.error')
  case "$e" in *"$5"*) m=yes;; *) m="error '$e'";; esac
  check "$1" "$(onejson "$2")|$(jq1 "$2" .ok)|$3|$m" "yes|false|$4|yes"; }
wait_up() { for _ in $(seq 180); do P -c "SELECT 1" >/dev/null 2>&1 && return 0; sleep 2; done; return 1; }

wait_up || { echo "server not up"; exit 1; }
P -c "DROP DATABASE IF EXISTS cli_copy_dummy" >/dev/null
P -d shop -c "SELECT 1" >/dev/null 2>&1 || { P -c "CREATE DATABASE shop"; sleep 10; }

echo "## a. doctor"
out=$(J doctor); rc=$?
check "doctor --json healthy, exit 0" "$(onejson "$out")|$(jq1 "$out" .healthy)|$rc" "yes|true|0"
check "doctor(): every check ok" "$(P -c "SELECT count(*) FILTER (WHERE NOT ok) FROM pgbx.doctor()")" 0
P -c "DROP ROLE IF EXISTS cli_viewer" -c "CREATE ROLE cli_viewer LOGIN IN ROLE pgbx_viewer" >/dev/null
check "doctor() callable as a viewer" "$(X psql -U cli_viewer -d postgres -qAt -c "SELECT count(*) > 0 FROM pgbx.doctor()" 2>&1)" t

echo "## b. policy commands (--json)"
out=$(J schedule 'every 2 hours' --db shop); check "schedule set" "$(jq1 "$out" .ok)" true
out=$(J schedule --db shop); check "schedule shows it" "$(jq1 "$out" .schedule.schedule)|$(jq1 "$out" .safety)" "every 2 hours|readonly"
out=$(J retention --db shop --max-backups 10 --max-days 60); check "raising retention needs no --yes" "$(jq1 "$out" .ok)" true
out=$(J retention --db shop); check "retention shows it" "$(jq1 "$out" '"\(.max_backups)/\(.max_days)"')" "10/60"
out=$(J retention --db shop --max-backups 5); rc=$?; refused "lowering retention refused without --yes" "$out" $rc 1 "--yes"
out=$(J retention --db shop --max-backups 5 --yes); check "lowering retention with --yes" "$(jq1 "$out" .ok)" true
out=$(J pause --db shop --reason cli-test); rc=$?; refused "pause refused without --yes" "$out" $rc 1 "--yes"
out=$(J pause --db shop --reason cli-test --yes); check "pause with --yes" "$(jq1 "$out" .ok)" true
check "status paused" "$(P -d shop -c "SELECT state FROM pgbx.status()")" paused
out=$(J resume --db shop); check "resume" "$(jq1 "$out" .ok)|$(P -d shop -c "SELECT state<>'paused' FROM pgbx.status()")" "true|t"
out=$(J scope --db shop --exclude public.nothing_here); rc=$?; refused "scope --exclude refused without --yes" "$out" $rc 1 "narrowing"
out=$(J scope --db shop --exclude public.nothing_here --yes); check "scope --exclude with --yes" "$(jq1 "$out" .ok)" true
out=$(J scope --db shop --include 'billing.*' --yes); check "scope --include with --yes" "$(jq1 "$out" .ok)" true
out=$(J scope --db shop); check "scope shows it" "$(jq1 "$out" .data_scope | grep -c 'billing')" 1
out=$(J scope --db shop --reset); check "scope --reset (widening) needs no --yes" "$(jq1 "$out" .ok)" true
out=$(J scope --db shop); check "scope after reset" "$(jq1 "$out" '.rowless_tables|length')" 0
out=$(J verify-schedule never --db shop); rc=$?; refused "verify-schedule never refused without --yes" "$out" $rc 1 "--yes"
out=$(J verify-schedule never --db shop --yes); check "verify-schedule never with --yes" "$(jq1 "$out" .ok)" true
out=$(J verify-schedule 'weekly on sunday at 04:00' --db shop); check "verify-schedule back on" "$(jq1 "$out" .ok)" true
out=$(J now --db shop --wait --timeout 300); check "now --db shop --wait" "$(jq1 "$out" .ok)|$(jq1 "$out" .state)" "true|done"
out=$(J verify --db shop --wait --timeout 300); check "verify --db shop --wait" "$(jq1 "$out" .ok)|$(jq1 "$out" .state)" "true|done"
out=$(J link --db shop --expires '10 minutes'); url=$(jq1 "$out" .url)
check "link: fetchable from the host (HTTP 200)" "$(curl -s -o /dev/null -w '%{http_code}' "$url")" 200
out=$(J overview); check "overview lists shop" "$(jq1 "$out" '[.databases[].database] | index("shop") != null')" true
out=$(J list --db shop); check "list" "$(jq1 "$out" .ok)" true
out=$(J jobs); check "jobs: the server-wide queue" "$(jq1 "$out" .ok)|$(jq1 "$out" '.jobs|type')|$(jq1 "$out" .safety)" "true|array|readonly"
out=$(J jobs cancel 999999 --db shop); rc=$?; refused "jobs cancel needs --yes" "$out" $rc 1 "--yes"

echo "## c. refusals (one JSON object, exit 1; usage errors exit 2)"
out=$(J db-restore --db shop --into shop); rc=$?; refused "db-restore into the source" "$out" $rc 1 "NEW database"
out=$(J db-restore --db shop --into shop_x --time '2026-01-01 00:00:00'); rc=$?; refused "--time without UTC offset" "$out" $rc 1 "UTC offset"
out=$(J frobnicate); rc=$?; check "unknown command: JSON usage error, exit 2" "$(onejson "$out")|$(jq1 "$out" .ok)|$rc" "yes|false|2"
out=$(J now --bogus-flag); rc=$?; check "unknown flag: JSON usage error, exit 2" "$(onejson "$out")|$(jq1 "$out" .ok)|$rc" "yes|false|2"

echo "## d. pgbx query (read ones of pgbx.* allowed; the rest refused; more in tests/ssh_e2e.sh)"
out=$(J query "SELECT state FROM pgbx.status()" --db shop); check "query pgbx.status()" "$(jq1 "$out" .ok)|$(jq1 "$out" .row_count)" "true|1"
out=$(J query "SELECT pgbx.backup_now()" --db shop); rc=$?; refused "query pgbx.backup_now() refused" "$out" $rc 1 "side effects"
out=$(J query "DELETE FROM pgbx.history" --db shop); rc=$?; refused "query DELETE refused" "$out" $rc 1 "SELECT-style"

echo "## e. setup client against this server (direct)"
SC() { $DC exec -T -u postgres -e PGBX_CONFIG_DIR=/tmp/pgbx-sc db pgbx setup client "$@" --json 2>/dev/null; }
X rm -rf /tmp/pgbx-sc
out=$(SC local --host /var/run/postgresql --user postgres --db shop --yes); rc=$?
check "setup client: ok, extension found, status read" "$(jq1 "$out" .ok)|$rc|$(jq1 "$out" '.test.extension_version != null')|$(jq1 "$out" '.test.status.state != null')" "true|0|true|true"
check "setup client: first profile is the default" "$(jq1 "$out" .profile.default)" true
out=$(X pgbx setup --json 2>/dev/null); check "plain setup is the server alias" "$(jq1 "$out" .hint)" "same as pgbx setup server"
X rm -rf /tmp/pgbx-sc

echo "## h. agent skill"
X sh -c 'rm -rf /tmp/skh; mkdir -p /tmp/skh/claude; ln -s /tmp /tmp/skh/claude/foreign'
SK() { $DC exec -T -u postgres -e HOME=/tmp/skh -e CLAUDE_SKILLS_DIR=/tmp/skh/claude db pgbx skill "$@" --no-codex --json 2>/dev/null; }
out=$(SK install); check "skill install" "$(jq1 "$out" .ok)" true
check "link points at a dir with SKILL.md" "$(X sh -c 'test -L /tmp/skh/claude/pgbx-skill && test -f /tmp/skh/claude/pgbx-skill/SKILL.md && echo yes')" yes
out=$(SK where); check "where: versions match" "$(jq1 "$out" .same_version)|$(jq1 "$out" '.installed_version == .binary_version')" "true|true"
out=$(SK uninstall); check "skill uninstall" "$(jq1 "$out" .ok)" true
check "uninstall removed only ours" "$(X sh -c 'test -e /tmp/skh/claude/pgbx-skill && echo ours-left; test -L /tmp/skh/claude/foreign && echo foreign-kept')" foreign-kept
X rm -rf /tmp/skh

echo "## j. new server: db-restore --from-s3 onto a plain Postgres WITHOUT the extension"
PLAIN=pgbx-cli-plain
NET=$(docker inspect -f '{{range $k, $v := .NetworkSettings.Networks}}{{$k}} {{end}}' "$($DC ps -q db)" | awk '{print $1}')
docker rm -f $PLAIN >/dev/null 2>&1
docker run -d --name $PLAIN --network "$NET" -e POSTGRES_HOST_AUTH_METHOD=trust "postgres:${PG_MAJOR:-16}-bookworm" >/dev/null
PP() { docker exec -u postgres $PLAIN psql -qAt "$@" 2>/dev/null; }
for _ in $(seq 60); do [ "$(PP -c 'SELECT 1')" = 1 ] && docker logs $PLAIN 2>&1 | grep -q "init process complete" && break; sleep 1; done
for _ in $(seq 30); do [ "$(PP -c 'SELECT 1')" = 1 ] && break; sleep 1; done
check "plain server has no pgbx available" "$(PP -c "SELECT count(*) FROM pg_available_extensions WHERE name = 'pgbx'")" 0
ep=$(P -c "SHOW pgbx.s3_endpoint"); bk=$(P -c "SHOW pgbx.s3_bucket"); rg=$(P -c "SHOW pgbx.s3_region"); sv=$(P -c "SHOW pgbx.server_name")
S3="--s3-endpoint $ep --s3-bucket $bk --s3-region $rg --server-name $sv --credentials-file /etc/pgbx/s3.credentials"
T0=$(P -c "SELECT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') || '+00'"); sleep 2
P -d shop -c "DROP TABLE IF EXISTS cli_dr; CREATE TABLE cli_dr AS SELECT g n FROM generate_series(1,123) g" >/dev/null
out=$(J now --db shop --wait --timeout 300); check "fresh backup of shop" "$(jq1 "$out" .state)" done
newest=$(P -d shop -c "SELECT last_backup_key FROM pgbx.status()")
out=$(J backups --from-s3 --db shop $S3); rc=$?
check "backups --from-s3: ok, newest first = status()'s last key" "$(onejson "$out")|$rc|$(jq1 "$out" '.backups[0].key')" "yes|0|$newest"
check "backups --from-s3 never prints keys" "$(echo "$out" | grep -c -i 'secret\|access_key')" 0
out=$(J backups --db shop); rc=$?; refused "backups without --from-s3" "$out" $rc 1 "--from-s3"
out=$(J db-restore --from-s3 --db shop --into shop_dr --host $PLAIN --s3-endpoint "$ep"); rc=$?; refused "missing S3 flags" "$out" $rc 1 "is required"
out=$(J db-restore --from-s3 --db shop --into shop_dr --host $PLAIN --time '2026-01-01 00:00:00' $S3); rc=$?; refused "--from-s3 --time without offset" "$out" $rc 1 "UTC offset"
out=$(J db-restore --from-s3 --db shop --into shop_dr --host $PLAIN --time '2000-01-01 00:00:00+00' $S3); rc=$?; refused "--time before the first backup" "$out" $rc 1 "no backup at or before"
out=$(J db-restore --from-s3 --db no_such_db --into shop_dr --host $PLAIN $S3); rc=$?; refused "empty folder" "$out" $rc 1 "no backups"
PP -c "CREATE DATABASE taken" >/dev/null
out=$(J db-restore --from-s3 --db shop --into taken --host $PLAIN $S3); rc=$?; refused "--into an existing database" "$out" $rc 1 "already exists"
check "refusals created nothing" "$(PP -c "SELECT count(*) FROM pg_database WHERE datname LIKE 'shop_dr%'")" 0
out=$(J db-restore --from-s3 --db shop --into shop_dr --host $PLAIN $S3); rc=$?
check "db-restore --from-s3 (newest): ok, exit 0, key" "$(jq1 "$out" .ok)|$rc|$(jq1 "$out" .key)" "true|0|$newest"
[ "$(jq1 "$out" .ok)" = true ] || echo "$out" | head -c 1500
check "rows restored on the plain server" "$(PP -d shop_dr -c 'SELECT count(*) FROM cli_dr')" 123
check "no extension objects on the plain server" "$(PP -d shop_dr -c "SELECT count(*) FROM pg_namespace WHERE nspname = 'pgbx'")" 0
out=$(J db-restore --from-s3 --db shop --into shop_dr --host $PLAIN $S3); rc=$?; refused "second restore into the same name" "$out" $rc 1 "already exists"
out=$(J db-restore --from-s3 --db shop --into shop_dr_t0 --host $PLAIN --time "$T0" $S3); rc=$?
check "--time before cli_dr existed: ok" "$(jq1 "$out" .ok)|$rc" "true|0"
check "that restore predates the table" "$(PP -d shop_dr_t0 -c "SELECT count(*) FROM pg_tables WHERE tablename = 'cli_dr'")" 0
oldest=$(J backups --from-s3 --db shop $S3 | jq -r '.backups[-1].key')
out=$(J db-restore --from-s3 --db shop --into shop_dr_k --host $PLAIN --backup "${oldest##*/}" $S3)
check "--backup <file name> restores that exact dump" "$(jq1 "$out" .ok)|$(jq1 "$out" .key)" "true|$oldest"

echo "## k. profiles (stored in a scratch config dir; never hold secrets)"
PJ() { $DC exec -T -u postgres -e PGBX_CONFIG_DIR=/tmp/pgbx-prof db pgbx "$@" --json 2>/dev/null; }
X rm -rf /tmp/pgbx-prof
out=$(PJ profile add plain --host "$PLAIN" $S3); check "profile add" "$(jq1 "$out" .ok)|$(jq1 "$out" .profile.default)" "true|true"
check "profile file is 0600 and has no key values" "$(X sh -c 'stat -c %a /tmp/pgbx-prof/profiles.json; grep -ci secret_access_key /tmp/pgbx-prof/profiles.json')" "600
0"
out=$(PJ profile list); check "profile list" "$(jq1 "$out" '.profiles|length')|$(jq1 "$out" .default)" "1|plain"
out=$(PJ backups --from-s3 --db shop --profile plain); check "backups --from-s3 via --profile" "$(jq1 "$out" .ok)|$(jq1 "$out" .profile_used)|$(jq1 "$out" '.backups[0].key')" "true|plain|$newest"
out=$(PJ status --db shop --profile nope); rc=$?; refused "unknown --profile" "$out" $rc 1 "no profile"
out=$(PJ profile remove plain); check "profile remove" "$(jq1 "$out" .ok)" true
X rm -rf /tmp/pgbx-prof
docker rm -f $PLAIN >/dev/null 2>&1

echo "RESULT: $pass passed, $fail failed"; [ $fail -eq 0 ]
