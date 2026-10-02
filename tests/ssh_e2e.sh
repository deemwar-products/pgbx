#!/usr/bin/env bash
# End-to-end check of pgbx over SSH and pgbx query, against throwaway containers on this machine:
# a plain postgres (no extension needed) and an Alpine sshd next to it on one docker network.
# The host's pgbx (cli/target/debug/pgbx, built if missing) reaches Postgres only through the ssh forward.
# Run: tests/ssh_e2e.sh     (needs docker, ssh, jq; bash 3.2-safe; exits non-zero on any failure)
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
B=${PGBX_BIN:-$ROOT/cli/target/debug/pgbx}
[ -x "$B" ] || cargo build -q --manifest-path "$ROOT/cli/Cargo.toml" || exit 1
NET=pgbx-ssh-net; PG=pgbx-ssh-pg; SSHD=pgbx-ssh-sshd; SPORT=${SSH_TEST_PORT:-22222}
W=$(mktemp -d)
export PGBX_CONFIG_DIR=$W/config PGBX_STATE_DIR=$W/state PGBX_SSH=$W/ssh
cleanup() { "$B" tunnel close --all >/dev/null 2>&1; docker rm -f $PG $SSHD >/dev/null 2>&1; docker network rm $NET >/dev/null 2>&1; rm -rf "$W"; }
trap cleanup EXIT
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "  PASS $1"; pass=$((pass+1)); else echo "  FAIL $1 (got '$2', want '$3')"; fail=$((fail+1)); fi; }
jq1() { echo "$1" | jq -r "$2" 2>/dev/null; }
J() { "$B" "$@" --json 2>/dev/null; }
port_of() { J tunnel list | jq -r '.tunnels[] | select(.name=="t") | .local_port'; }

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
docker run -d --name $PG --network $NET -e POSTGRES_HOST_AUTH_METHOD=trust postgres:16-alpine >/dev/null
docker run -d --name $SSHD --network $NET -p "127.0.0.1:$SPORT:22" -e KEY="$(cat "$W/key.pub")" alpine:3.20 sh -c '
  apk add -q --no-cache openssh >/dev/null && ssh-keygen -A >/dev/null && mkdir -p /root/.ssh &&
  echo "$KEY" > /root/.ssh/authorized_keys && chmod 600 /root/.ssh/authorized_keys &&
  sed -i "s/^#*AllowTcpForwarding.*/AllowTcpForwarding yes/" /etc/ssh/sshd_config && passwd -u root >/dev/null 2>&1;
  exec /usr/sbin/sshd -D -e' >/dev/null
for _ in $(seq 60); do "$W/ssh" pgbxtest true 2>/dev/null && break; sleep 1; done
for _ in $(seq 60); do docker exec $PG pg_isready -q -U postgres 2>/dev/null && break; sleep 1; done
docker exec $PG psql -qU postgres -c "CREATE TABLE t AS SELECT g AS n, 'row '||g AS s FROM generate_series(1,50) g" >/dev/null

echo "## a. profile with ssh; the tunnel is created once and reused"
out=$(J profile add t --ssh pgbxtest --host $PG --user postgres --tunnel-idle 4s); check "profile add (ssh)" "$(jq1 "$out" .ok)" true
out=$(J query "SELECT count(*) AS n, true AS b, null::text AS z, 1.5::numeric AS x FROM t")
check "query through ssh: typed values" "$(jq1 "$out" '"\(.rows[0].n)|\(.rows[0].b)|\(.rows[0].z)|\(.rows[0].x)|\(.columns[0].type)"')" "50|true|null|1.5|int8"
check "profile_used" "$(jq1 "$out" .profile_used)" t
p1=$(port_of); check "tunnel listed" "$([ -n "$p1" ] && echo yes)" yes
out=$(J query "SELECT 1 AS one"); check "second query ok" "$(jq1 "$out" '.rows[0].one')" 1
check "second command reuses the same port" "$(port_of)" "$p1"
check "only one tunnel" "$(J tunnel list | jq '.tunnels|length')" 1
check "state file is 0600" "$(stat -f %Lp "$PGBX_STATE_DIR/tunnels/t.json" 2>/dev/null || stat -c %a "$PGBX_STATE_DIR/tunnels/t.json")" 600
sleep 7
check "idle tunnel is gone after tunnel-idle" "$(J tunnel list | jq '.tunnels|length')" 0
out=$(J query "SELECT 2 AS two"); check "next command recreates it" "$(jq1 "$out" '.rows[0].two')|$([ -n "$(port_of)" ] && echo up)" "2|up"
out=$(J tunnel); check "pgbx tunnel prints the local port" "$(jq1 "$out" .local_port)" "$(port_of)"
out=$(J tunnel close t); check "tunnel close" "$(jq1 "$out" '.closed[0]')|$(J tunnel list | jq '.tunnels|length')" "t|0"

echo "## b. pgbx query guard (best effort, not a security boundary)"
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

echo "## c. host-side commands run over ssh"
out=$(J doctor); check "doctor on a host without pgbx: install hint" "$(jq1 "$out" .error | grep -c 'install.sh')" 1

echo "RESULT: $pass passed, $fail failed"; [ $fail -eq 0 ]
