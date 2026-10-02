#!/usr/bin/env bash
# End-to-end check of connections (ADR 0003) and pgbx query, against throwaway containers on this machine:
# a plain postgres with PASSWORD auth (no extension needed) and an Alpine sshd next to it on one docker network.
#   a. a url profile with $PGPASSWORD: expanded at run time from the environment, a .env file or a secret handler;
#      a literal password is refused; nothing secret on disk or in any output
#   b. the ssh example adapter (node adapters/ssh/ssh-adapter.js): Postgres only through the ssh forward, started
#      per command and gone afterwards; host-side commands say to run on the host; adapter failures are shown
#   c. the pgbx query guard   d. setup client with an adapter   e. profiles.json from 0.5 is migrated
# Run: tests/ssh_e2e.sh     (needs docker, ssh, node 18+, jq; bash 3.2-safe; exits non-zero on any failure)
set -u -o pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
B=${PGBX_BIN:-$ROOT/cli/target/debug/pgbx}
[ -n "${PGBX_BIN:-}" ] || cargo build -q --manifest-path "$ROOT/cli/Cargo.toml" || exit 1   # always the current source
NET=pgbx-ssh-net; PG=pgbx-ssh-pg; SSHD=pgbx-ssh-sshd; SPORT=${SSH_TEST_PORT:-22222}; PPORT=${SSH_TEST_PG_PORT:-55498}
W=$(mktemp -d)
PW="e2e-Pw-$$-$(date +%s)-x"          # the database password: must never show up in output or under the config dir
ADAPTER="node $ROOT/adapters/ssh/ssh-adapter.js"
export PGBX_CONFIG_DIR=$W/config PGBX_SSH_BIN=$W/ssh PGBX_MEMORY_DIR=$W/mem
unset PGPASSWORD PGBX_URL PGBX_PROFILE
cleanup() { docker rm -f $PG $SSHD >/dev/null 2>&1; docker network rm $NET >/dev/null 2>&1; pkill -f "$W/ssh_config" 2>/dev/null; rm -rf "$W"; }
trap cleanup EXIT
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "  PASS $1"; pass=$((pass+1)); else echo "  FAIL $1 (got '$2', want '$3')"; echo "       last output: $(printf %s "${out:-}" | head -c 400)"; fail=$((fail+1)); fi; }
jq1() { echo "$1" | jq -r "$2" 2>/dev/null; }
J() { "$B" "$@" --json 2>>"$W/stderr.log" | tee -a "$W/stdout.log"; }   # every output is kept and scanned at the end
ssh_left() { pgrep -f "$W/ssh_config" | wc -l | tr -d ' '; }            # ssh forwards still running

ssh-keygen -q -t ed25519 -N '' -f "$W/key"
cat > "$W/ssh_config" <<CFG
Host pgbxtest
  HostName 127.0.0.1
  Port $SPORT
  User root
  IdentityFile $W/key
  IdentitiesOnly yes
  StrictHostKeyChecking no
  UserKnownHostsFile /dev/null
  LogLevel ERROR
CFG
printf '#!/bin/sh\nexec ssh -F %s "$@"\n' "$W/ssh_config" > "$W/ssh"; chmod +x "$W/ssh"

docker rm -f $PG $SSHD >/dev/null 2>&1; docker network rm $NET >/dev/null 2>&1
docker network create $NET >/dev/null
docker run -d --name $PG --network $NET -p "127.0.0.1:$PPORT:5432" -e POSTGRES_PASSWORD="$PW" postgres:16-alpine >/dev/null
docker run -d --name $SSHD --network $NET -p "127.0.0.1:$SPORT:22" -e KEY="$(cat "$W/key.pub")" alpine:3.20 sh -c '
  apk add -q --no-cache openssh >/dev/null && ssh-keygen -A >/dev/null && mkdir -p /root/.ssh &&
  echo "$KEY" > /root/.ssh/authorized_keys && chmod 600 /root/.ssh/authorized_keys &&
  sed -i "s/^#*AllowTcpForwarding.*/AllowTcpForwarding yes/" /etc/ssh/sshd_config && passwd -u root >/dev/null 2>&1;
  exec /usr/sbin/sshd -D -e' >/dev/null
for _ in $(seq 60); do "$W/ssh" pgbxtest true 2>/dev/null && break; sleep 1; done
for _ in $(seq 60); do docker logs $PG 2>&1 | grep -q "init process complete" && docker exec $PG pg_isready -q -U postgres 2>/dev/null && break; sleep 1; done
for _ in $(seq 30); do docker exec $PG psql -qAtU postgres -c "SELECT 1" >/dev/null 2>&1 && break; sleep 1; done
docker exec $PG psql -qU postgres -c "CREATE TABLE t AS SELECT g AS n, 'row '||g AS s FROM generate_series(1,50) g" >/dev/null
URL="postgres://postgres:\$PGPASSWORD@127.0.0.1:$PPORT/postgres"

echo "## a. url profile + \$PGPASSWORD (environment, .env file, secret handler)"
out=$(J profile add direct --url "$URL"); check "profile add --url with a \$VAR reference" "$(jq1 "$out" .ok)|$(jq1 "$out" .profile.connection)" "true|url"
out=$(J profile add leak --url "postgres://postgres:$PW@127.0.0.1:$PPORT/postgres"); rc=$?
check "a literal password is refused" "$(jq1 "$out" .ok)|$(jq1 "$out" .error | grep -c 'refusing to store a password')" "false|1"
check "config.yaml is 0600" "$(stat -f %Lp "$PGBX_CONFIG_DIR/config.yaml" 2>/dev/null || stat -c %a "$PGBX_CONFIG_DIR/config.yaml")" 600
check "config.yaml holds the reference" "$(grep -c 'PGPASSWORD' "$PGBX_CONFIG_DIR/config.yaml")" 1
out=$(J query "SELECT count(*) AS n FROM t"); rc=$?
check "PGPASSWORD unset: the error names the variable" "$(jq1 "$out" .error | grep -c '\$PGPASSWORD is not set')|$rc" "1|1"
out=$(PGPASSWORD=$PW J query "SELECT count(*) AS n FROM t")
check "PGPASSWORD set: expanded at run time" "$(jq1 "$out" '.rows[0].n')|$(jq1 "$out" .profile_used)" "50|direct"
out=$(PGPASSWORD=wrong-$PW J query "SELECT 1 AS one"); rc=$?
check "wrong password: auth error, exit 1" "$(jq1 "$out" .error | grep -c 'password authentication failed')|$rc" "1|1"
check "the wrong password is not echoed" "$(echo "$out" | grep -c "wrong-$PW")" 0
out=$(PGPASSWORD=$PW J query "SELECT 2 AS two" --url "$URL"); check "--url for one run" "$(jq1 "$out" '.rows[0].two')|$(jq1 "$out" .profile_used)" "2|null"
out=$(PGPASSWORD=$PW PGBX_URL=$URL J query "SELECT 3 AS three"); check "PGBX_URL for one run" "$(jq1 "$out" '.rows[0].three')" 3
out=$(J query "SELECT 1" --url "postgres://postgres:\$\$x@127.0.0.1:$PPORT/postgres"); check "\$\$ is a literal \$ (sent as the password)" "$(jq1 "$out" .error | grep -c 'password authentication failed')" 1
mkdir -p "$W/secrets" && chmod 700 "$W/secrets"
printf '# the database\nDB_PW="%s"\n' "$PW" > "$W/secrets/db.env"; chmod 600 "$W/secrets/db.env"
sed -i.bak "s|^secrets: .*|secrets: $W/secrets/db.env|" "$PGBX_CONFIG_DIR/config.yaml"; rm -f "$PGBX_CONFIG_DIR/config.yaml.bak"
out=$(J profile add viaenv --url "postgres://postgres:\$DB_PW@127.0.0.1:$PPORT/postgres"); check "profile with \$DB_PW" "$(jq1 "$out" .ok)" true
out=$(J query "SELECT 4 AS four" --profile viaenv); check "secrets: a .env file" "$(jq1 "$out" '.rows[0].four')" 4
printf '#!/bin/sh\n[ "$1" = DB_PW ] && printf "%%s\\n" "%s" && exit 0\nexit 2\n' "$PW" > "$W/secrets/handler.sh"; chmod 700 "$W/secrets/handler.sh"
sed -i.bak "s|^secrets: .*|secrets: sh $W/secrets/handler.sh|" "$PGBX_CONFIG_DIR/config.yaml"; rm -f "$PGBX_CONFIG_DIR/config.yaml.bak"
out=$(J query "SELECT 5 AS five" --profile viaenv); check "secrets: a handler command" "$(jq1 "$out" '.rows[0].five')" 5
out=$(J query "SELECT 1" --url "postgres://postgres:\$NOT_THERE@127.0.0.1:$PPORT/postgres")
check "handler fails for an unknown name: named, never valued" "$(jq1 "$out" .error | grep -c 'failed for \$NOT_THERE')" 1
sed -i.bak "s|^secrets: .*|secrets: env|" "$PGBX_CONFIG_DIR/config.yaml"; rm -f "$PGBX_CONFIG_DIR/config.yaml.bak"
export PGPASSWORD=$PW   # from here on, the environment holds it

echo "## b. the ssh example adapter"
out=$(J profile add t --adapter ssh --adapter-command "$ADAPTER" target=pgbxtest pg_host=$PG user=postgres 'password=$PGPASSWORD')
check "profile add --adapter ssh: prints what it runs" "$(jq1 "$out" .ok)|$(jq1 "$out" .profile.runs | grep -c 'ssh-adapter.js')|$(jq1 "$out" '.notices[0]' | grep -c 'runs:')" "true|1|1"
out=$(J query "SELECT count(*) AS n, true AS b, null::text AS z, 1.5::numeric AS x FROM t" --profile t)
check "query through ssh: typed values" "$(jq1 "$out" '"\(.rows[0].n)|\(.rows[0].b)|\(.rows[0].z)|\(.rows[0].x)|\(.columns[0].type)"')" "50|true|null|1.5|int8"
check "profile_used" "$(jq1 "$out" .profile_used)" t
check "the ssh forward is gone after the command" "$(ssh_left)" 0
J profile use t >/dev/null
out=$(J query "SELECT 1 AS one"); check "default profile: next command starts it again" "$(jq1 "$out" '.rows[0].one')|$(ssh_left)" "1|0"
out=$(J status); check "status through the adapter: backups off" "$(jq1 "$out" .backups)" off
out=$(J doctor); rc=$?
check "doctor: SQL checks through the adapter, healthy" "$(jq1 "$out" .healthy)|$rc|$(jq1 "$out" '.checks[] | select(.name=="backups (pgbx extension)") | .ok')" "true|0|true"
check "doctor: says host-side checks run on the host" "$(jq1 "$out" '.checks[] | select(.name=="host-side checks") | .info' | grep -c 'on the database host')" 1
out=$(J diagnose); rc=$?; check "diagnose with an adapter profile: run it on the host" "$(jq1 "$out" .error | grep -c 'on the database host')|$rc" "1|1"
out=$(J setup server --yes); check "setup server with an adapter profile: run it on the host" "$(jq1 "$out" .error | grep -c 'on the database host')" 1
out=$(J logs); check "logs (no extension): one plain sentence" "$(jq1 "$out" .error | grep -c '^pgbx extension not installed')" 1
J profile add badssh --adapter ssh target=nosuchhost.invalid ready_timeout_ms=3000 >/dev/null
out=$(J query "SELECT 1" --profile badssh); rc=$?
check "adapter error: state shown with its stderr, exit 1" "$(jq1 "$out" .error | grep -c 'adapter state: error')|$(jq1 "$out" .error | grep -c 'adapter stderr')|$rc" "1|1|1"
check "nothing left running after the failure" "$(ssh_left)" 0
J profile remove badssh >/dev/null

echo "## c. pgbx query guard (best effort, not a security boundary)"
out=$(J query "SELECT n FROM t ORDER BY n" --max-rows 10); check "row cap" "$(jq1 "$out" '"\(.row_count)|\(.truncated)"')" "10|true"
out=$(J query "SELECT n FROM t" --max-rows 50); check "no truncation at the exact count" "$(jq1 "$out" '"\(.row_count)|\(.truncated)"')" "50|false"
out=$(J query "SHOW server_version_num"); check "SHOW works" "$(jq1 "$out" '.rows[0].server_version_num | tostring | .[0:2]')" 16
out=$(J query "INSERT INTO t VALUES (99)"); rc=$?; check "INSERT refused" "$(jq1 "$out" .ok)|$rc" "false|1"
out=$(J query "SELECT 1; SELECT 2"); check "multi-statement refused" "$(jq1 "$out" .error | grep -c 'exactly one')" 1
out=$(J query "WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d"); check "data-modifying WITH refused" "$(jq1 "$out" .ok)" false
out=$(J query "SELECT pg_terminate_backend(1)"); check "side-effect function refused" "$(jq1 "$out" .error | grep -c 'side effects')" 1
out=$(J query "SELECT pg_sleep(5)" --timeout 1s); check "pg_sleep refused" "$(jq1 "$out" .ok)" false
out=$(J query "SELECT count(*) FROM generate_series(1,100000000)" --timeout 1s); check "statement_timeout applies" "$(jq1 "$out" .error | grep -c 'statement timeout')" 1
check "nothing was written" "$(docker exec $PG psql -qAtU postgres -c 'SELECT count(*) FROM t')" 50

echo "## d. setup client with an adapter (no sudo; writes a profile, then tests it)"
out=$(J setup client viassh --adapter ssh target=pgbxtest pg_host=$PG 'password=$PGPASSWORD')
check "without --yes: plan only" "$(jq1 "$out" .ok)|$(jq1 "$out" .plan.profile)|$(J profile list | jq '[.profiles[].name] | index("viassh")')" "false|viassh|null"
out=$(J setup client viassh --adapter ssh target=pgbxtest pg_host=$PG 'password=$PGPASSWORD' --yes); rc=$?
check "over the ssh adapter: connects, no extension = backups off, exit 0" "$(jq1 "$out" .test.connected)|$(jq1 "$out" .test.backups)|$rc" "true|off|0"
check "optional next step names install.sh and setup server" "$(jq1 "$out" '.next_steps[0]' | grep -c '^optional.*install.sh.*pgbx setup server')" 1
check "profile saved, the adapter stopped after the test" "$(J profile show viassh | jq -r .profile.settings.target)|$(ssh_left)" "pgbxtest|0"
check "no terminal: skill not installed" "$(jq1 "$out" .skill.installed)" false
out=$(J setup client bad --host 127.0.0.1 --port 1 --yes); check "unreachable: clear next step" "$(jq1 "$out" .ok)|$(jq1 "$out" '.next_steps[0]' | grep -c 'cannot reach')" "false|1"
J profile remove viassh >/dev/null; J profile remove bad >/dev/null

echo "## e. a 0.5 profiles.json is migrated once (ssh profiles -> the ssh adapter)"
M=$W/old; mkdir -p "$M"
printf '{"default":"old","profiles":{"old":{"ssh":"pgbxtest","host":"%s","user":"postgres","tunnel-idle":"5m"},"plain":{"host":"127.0.0.1","port":"%s","user":"postgres"}}}\n' "$PG" "$PPORT" > "$M/profiles.json"
out=$(PGBX_CONFIG_DIR=$M PGBX_ADAPTERS_DIR=$ROOT/adapters "$B" profile list --json 2>"$W/mig.err")
check "migrated: the user is told once" "$(jq1 "$out" .notices[0] | grep -c "moved 2 profile")|$(jq1 "$out" .notices[1] | grep -c "no longer has built-in SSH")" "1|1"
check "ssh profile became adapter: ssh, the plain one a url" "$(jq1 "$out" '.profiles[] | select(.name=="old") | .settings.adapter')|$(jq1 "$out" '.profiles[] | select(.name=="plain") | .settings.url')" "ssh|postgres://postgres@127.0.0.1:$PPORT/"
check "old file kept as profiles.json.migrated, new one 0600" "$([ -f "$M/profiles.json.migrated" ] && [ ! -f "$M/profiles.json" ] && echo yes)|$(stat -f %Lp "$M/config.yaml" 2>/dev/null || stat -c %a "$M/config.yaml")" "yes|600"
out=$(PGBX_CONFIG_DIR=$M PGBX_ADAPTERS_DIR=$ROOT/adapters "$B" query "SELECT max(n) AS m FROM t" --json 2>"$W/mig2.err")
check "the migrated ssh profile works (no notice the second time)" "$(jq1 "$out" '.rows[0].m')|$(wc -c < "$W/mig2.err" | tr -d ' ')" "50|0"

echo "## f. the removed built-in SSH says where it went"
out=$("$B" status --ssh pgbxtest --json 2>/dev/null); check "--ssh points at the ssh adapter" "$(jq1 "$out" .error | grep -c 'ssh adapter')" 1
"$B" tunnel list >/dev/null 2>"$W/t.err"; rc=$?; check "pgbx tunnel is gone" "$rc|$(grep -c 'no built-in SSH' "$W/t.err")" "2|1"

echo "## g. the password never reached output or disk"
check "not in any stdout" "$(grep -c -- "$PW" "$W/stdout.log")" 0
check "not in any stderr" "$(grep -c -- "$PW" "$W/stderr.log")" 0
check "not under the config dirs" "$(grep -rl -- "$PW" "$PGBX_CONFIG_DIR" "$M" 2>/dev/null | wc -l | tr -d ' ')" 0

echo "RESULT: $pass passed, $fail failed"; [ $fail -eq 0 ]
