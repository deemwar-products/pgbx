#!/usr/bin/env bash
# pgbx as a plain Postgres client, no backups: every client path against a STOCK postgres (no pgbx extension,
# no S3), directly and through an Alpine sshd that has no pgbx installed either (the ssh example adapter). Nothing that a client-only user
# runs may fail or print a scary error; the backup commands must refuse in one plain sentence.
# Run: tests/client_only_e2e.sh     (needs docker, ssh, node 18+, jq; bash 3.2-safe; exits non-zero on any failure)
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
B=${PGBX_BIN:-$ROOT/cli/target/debug/pgbx}
[ -x "$B" ] || cargo build -q --manifest-path "$ROOT/cli/Cargo.toml" || exit 1
NET=pgbx-co-net; PG=pgbx-co-pg; SSHD=pgbx-co-sshd; SPORT=${CO_SSH_PORT:-22223}; PPORT=${CO_PG_PORT:-55499}
W=$(mktemp -d)
export PGBX_CONFIG_DIR=$W/config PGBX_SSH_BIN=$W/ssh PGBX_MEMORY_DIR=$W/mem
unset PGBX_URL PGBX_PROFILE
ADAPTER="node $ROOT/adapters/ssh/ssh-adapter.js"
cleanup() { docker rm -f $PG $SSHD >/dev/null 2>&1; docker network rm $NET >/dev/null 2>&1; pkill -f "$W/ssh_config" 2>/dev/null; rm -rf "$W"; }
trap cleanup EXIT
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "  PASS $1"; pass=$((pass+1)); else echo "  FAIL $1 (got '$2', want '$3')"; fail=$((fail+1)); fi; }
jq1() { echo "$1" | jq -r "$2" 2>/dev/null; }
J() { "$B" "$@" --json 2>/dev/null; }
OFF="pgbx extension not installed on this server: backups are off; queries, profiles and adapters work"

ssh-keygen -q -t ed25519 -N '' -f "$W/key"
cat > "$W/ssh_config" <<CFG
Host pgbxco
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
docker run -d --name $PG --network $NET -p "127.0.0.1:$PPORT:5432" -e POSTGRES_HOST_AUTH_METHOD=trust postgres:16-alpine >/dev/null
docker run -d --name $SSHD --network $NET -p "127.0.0.1:$SPORT:22" -e KEY="$(cat "$W/key.pub")" alpine:3.20 sh -c '
  apk add -q --no-cache openssh >/dev/null && ssh-keygen -A >/dev/null && mkdir -p /root/.ssh &&
  echo "$KEY" > /root/.ssh/authorized_keys && chmod 600 /root/.ssh/authorized_keys &&
  sed -i "s/^#*AllowTcpForwarding.*/AllowTcpForwarding yes/" /etc/ssh/sshd_config && passwd -u root >/dev/null 2>&1;
  exec /usr/sbin/sshd -D -e' >/dev/null
for _ in $(seq 60); do "$W/ssh" pgbxco true 2>/dev/null && break; sleep 1; done
for _ in $(seq 60); do docker logs $PG 2>&1 | grep -q "init process complete" && docker exec $PG pg_isready -q -U postgres 2>/dev/null && break; sleep 1; done
for _ in $(seq 30); do docker exec $PG psql -qAtU postgres -c "SELECT 1" >/dev/null 2>&1 && break; sleep 1; done
docker exec $PG psql -qU postgres -c "CREATE TABLE t AS SELECT g AS n, 'row '||g AS s FROM generate_series(1,50) g" >/dev/null
check "stock server has no pgbx" "$(docker exec $PG psql -qAtU postgres -c "SELECT count(*) FROM pg_available_extensions WHERE name = 'pgbx'")" 0

echo "## a. setup client, direct (no extension = backups off, still ok)"
out=$(J setup client direct --host 127.0.0.1 --port $PPORT --user postgres --yes); rc=$?
check "setup client: ok, exit 0" "$(jq1 "$out" .ok)|$rc|$(jq1 "$out" .error)" "true|0|null"
check "setup client: connected, backups off" "$(jq1 "$out" .test.connected)|$(jq1 "$out" .test.backups)|$(jq1 "$out" .test.extension)" "true|off|absent"
check "setup client: says what works" "$(jq1 "$out" .test.info)" "$OFF"
check "setup client: next step is optional" "$(jq1 "$out" '.next_steps[0]' | grep -c '^optional.*pgbx setup server')" 1
"$B" setup client direct2 --host 127.0.0.1 --port $PPORT --user postgres --yes >"$W/o" 2>"$W/e"; rc=$?
check "setup client (text): exit 0, nothing on stderr" "$rc|$(wc -c < "$W/e" | tr -d ' ')" "0|0"
J profile remove direct2 >/dev/null
check "first profile is the default" "$(J profile list | jq -r .default)" direct

echo "## b. query, status, doctor (default profile)"
out=$(J query "SELECT count(*) AS n FROM t"); rc=$?
check "query: rows, exit 0, profile_used" "$(jq1 "$out" '.rows[0].n')|$rc|$(jq1 "$out" .profile_used)" "50|0|direct"
out=$(J query "SELECT 1; DELETE FROM t"); rc=$?; check "query guard still refuses" "$(jq1 "$out" .ok)|$rc" "false|1"
out=$(J status); rc=$?
check "status: ok, exit 0, backups off, friendly" "$(jq1 "$out" .ok)|$rc|$(jq1 "$out" .backups)|$(jq1 "$out" .info)" "true|0|off|$OFF"
"$B" status >"$W/o" 2>"$W/e"; rc=$?
check "status (text): exit 0, says backups are off" "$rc|$(grep -c 'backups: off' "$W/o")|$(wc -c < "$W/e" | tr -d ' ')" "0|1|0"
out=$(J doctor); rc=$?
check "doctor: healthy, exit 0" "$(jq1 "$out" .healthy)|$rc" "true|0"
check "doctor: extension check is info, not a failure" "$(jq1 "$out" '.checks[] | select(.name=="backups (pgbx extension)") | "\(.ok)|\(.detail)"')" "true|$OFF"
"$B" doctor >"$W/o" 2>&1; rc=$?
check "doctor (text): [info] line, no FAIL, healthy" "$rc|$(grep -c '^\[info\]' "$W/o")|$(grep -c FAIL "$W/o")|$(tail -1 "$W/o")" "0|1|0|healthy"

echo "## c. backup-only commands refuse plainly"
for c in list overview logs; do
  out=$(J $c); rc=$?
  check "$c: exit 1, plain sentence" "$(jq1 "$out" .ok)|$rc|$(jq1 "$out" .error | grep -c "^$OFF")" "false|1|1"
done

echo "## d. agent memory and skill (no server needed)"
out=$(J memories path); rc=$?
check "memories path: ok, exit 0, per connection" "$(jq1 "$out" .ok)|$rc|$(jq1 "$out" .dir)" "true|0|$W/mem/direct"
mkdir -p "$W/mem/direct/postgres" && printf '## row count of t\nSELECT count(*) FROM t\n' > "$W/mem/direct/postgres/memories.md"
out=$(J memories export "$W/m.json"); check "memories export" "$(jq1 "$out" .ok)|$(jq1 "$out" .files)" "true|1"
out=$(J memories import "$W/m.json" --as other); check "memories import --as" "$(jq1 "$out" .ok)|$(jq1 "$out" '.written[0]')" "true|postgres/memories.md"
mkdir -p "$W/home/claude"
SK() { HOME=$W/home CLAUDE_SKILLS_DIR=$W/home/claude "$B" skill "$@" --no-codex --json 2>/dev/null; }
out=$(SK install); check "skill install" "$(jq1 "$out" .ok)" true
out=$(SK uninstall); check "skill uninstall" "$(jq1 "$out" .ok)" true

echo "## e. through the ssh adapter to a host with no pgbx installed"
out=$(J setup client viassh --adapter ssh --adapter-command "$ADAPTER" target=pgbxco pg_host=$PG user=postgres --yes); rc=$?
check "setup client over ssh: ok, exit 0, backups off" "$(jq1 "$out" .ok)|$rc|$(jq1 "$out" .test.backups)" "true|0|off"
out=$(J query "SELECT max(n) AS m FROM t" --profile viassh); rc=$?
check "query through the ssh adapter" "$(jq1 "$out" '.rows[0].m')|$rc" "50|0"
out=$(J status --profile viassh); rc=$?
check "status through the ssh adapter: backups off, exit 0" "$(jq1 "$out" .backups)|$rc" "off|0"
out=$(J doctor --profile viassh); rc=$?
check "doctor: no pgbx on the host, SQL checks through the adapter" "$(jq1 "$out" .healthy)|$rc|$(jq1 "$out" '.checks[] | select(.name=="backups (pgbx extension)") | .ok')" "true|0|true"
check "no ssh forward outlives a command" "$(pgrep -f "$W/ssh_config" | wc -l | tr -d ' ')" 0

echo "RESULT: $pass passed, $fail failed"; [ $fail -eq 0 ]
