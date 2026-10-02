#!/usr/bin/env bash
# Real-world check of the ssh example adapter: a real sshd and a real Postgres in Docker, the real `ssh` client,
# and psql through the tunnel the adapter opens. Its own container names (pgbx-adp-*), no S3, cleans up after itself.
# Needs bash 4+ (coproc; on macOS: brew install bash), docker, jq, psql, nc.
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
NET=pgbx-adp-net; PG=pgbx-adp-pg; SSHD=pgbx-adp-sshd; SPORT=${SSH_TEST_PORT:-22223}
W=$(mktemp -d)
cleanup() { docker rm -f $PG $SSHD >/dev/null 2>&1; docker network rm $NET >/dev/null 2>&1; rm -rf "$W"; }
trap cleanup EXIT
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "  PASS $1"; pass=$((pass+1)); else echo "  FAIL $1 (got '$2', want '$3')"; fail=$((fail+1)); fi; }

ssh-keygen -q -t ed25519 -N '' -f "$W/key"
cat > "$W/ssh_config" <<CFG
Host adptest
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
docker run -d --name $PG --network $NET -e POSTGRES_PASSWORD='s3cret:p@ss' postgres:16-alpine >/dev/null
docker run -d --name $SSHD --network $NET -p "127.0.0.1:$SPORT:22" -e KEY="$(cat "$W/key.pub")" alpine:3.20 sh -c '
  apk add -q --no-cache openssh >/dev/null && ssh-keygen -A >/dev/null && mkdir -p /root/.ssh &&
  echo "$KEY" > /root/.ssh/authorized_keys && chmod 600 /root/.ssh/authorized_keys &&
  sed -i "s/^#*AllowTcpForwarding.*/AllowTcpForwarding yes/" /etc/ssh/sshd_config && passwd -u root >/dev/null 2>&1;
  exec /usr/sbin/sshd -D -e' >/dev/null
for _ in $(seq 60); do "$W/ssh" adptest true 2>/dev/null && break; sleep 1; done
for _ in $(seq 60); do docker exec $PG pg_isready -q -U postgres 2>/dev/null && break; sleep 1; done
docker exec $PG psql -qU postgres -c "CREATE TABLE t AS SELECT g AS n FROM generate_series(1,42) g" >/dev/null

echo "## ssh adapter: start -> url -> psql through the tunnel -> stop"
coproc AD { PGBX_SSH_BIN="$W/ssh" exec node "$ROOT/ssh/ssh-adapter.js" 2>"$W/stderr"; }
echo '{"action":"start","name":"prod","config":{"target":"adptest","pg_host":"'$PG'","user":"postgres","password":"s3cret:p@ss","dbname":"postgres"}}' >&"${AD[1]}"
IFS= read -r -t 30 line <&"${AD[0]}" || line=""
state=$(echo "$line" | jq -r .state); url=$(echo "$line" | jq -r .url)
check "result state" "$state" ready
check "result name" "$(echo "$line" | jq -r .name)" prod
check "rows through the tunnel" "$(psql "$url" -qAt -c 'SELECT count(*) FROM t' 2>&1)" 42
check "password never in adapter logs" "$(grep -c 's3cret' "$W/stderr")" 0
port=$(echo "$url" | sed -E 's#.*:([0-9]+)/.*#\1#')
echo '{"action":"stop"}' >&"${AD[1]}"
pid=$AD_PID; for _ in $(seq 50); do kill -0 "$pid" 2>/dev/null || break; sleep 0.1; done
check "adapter exited after stop" "$(kill -0 "$pid" 2>/dev/null && echo running || echo exited)" exited
check "tunnel closed after stop" "$(nc -z 127.0.0.1 "$port" 2>/dev/null && echo open || echo closed)" closed

echo "== ssh_real: $pass passed, $fail failed"
[ $fail -eq 0 ]
