#!/usr/bin/env bash
# TLS to Postgres with libpq's sslmode: a stock postgres:16 with ssl=on, a throwaway CA + server cert (for "localhost"),
# and a pg_hba.conf that only has hostssl lines (plain TCP is refused). Shows:
#   require / prefer / allow / the default connect over TLS (pg_stat_ssl.ssl = t), disable is refused by pg_hba,
#   verify-full fails with an unknown CA and works with sslrootcert (URL or PGSSLROOTCERT), verify-ca skips the name,
#   precedence (URL > profile key > PGSSLMODE), pgbx query and pgbx status through a url: profile with sslmode=require,
#   and the aws (RDS IAM auth) and azure (Entra, direct) example adapters, whose URLs say sslmode=require, against it
#   (their vendor CLIs replaced by a fake that prints a token and forwards the tunnel port).
# Run: tests/tls_e2e.sh     (needs docker, openssl, node 18+, jq; bash 3.2-safe; exits non-zero on any failure)
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
B=${PGBX_BIN:-$ROOT/cli/target/debug/pgbx}
[ -x "$B" ] || cargo build -q --manifest-path "$ROOT/cli/Cargo.toml" || exit 1
PG=pgbx-tls-pg; PPORT=${TLS_PG_PORT:-55497}
W=$(mktemp -d)
export PGBX_CONFIG_DIR=$W/config PGBX_MEMORY_DIR=$W/mem
unset PGBX_URL PGBX_PROFILE PGSSLMODE PGSSLROOTCERT PGHOST PGPORT PGUSER
# test-only passwords (a throwaway container); profiles reference them as $VARs, as pgbx requires
export PGBX_TLS_PW="tls-e2e-pw-$$"
TOKEN='fake-token/with+chars='
cleanup() { docker rm -fv $PG >/dev/null 2>&1; rm -rf "$W"; }
trap cleanup EXIT
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "  PASS $1"; pass=$((pass+1)); else echo "  FAIL $1 (got '$2', want '$3')"; fail=$((fail+1)); fi; }
jq1() { echo "$1" | jq -r "$2" 2>/dev/null; }
SSLQ="SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()"
U="postgres://postgres:$PGBX_TLS_PW@localhost:$PPORT/postgres"
U4="postgres://postgres:$PGBX_TLS_PW@127.0.0.1:$PPORT/postgres"
# ssl of this session: "true" / "false", or "ERR: <error>"
ssl() { local o; o=$("$B" query "$SSLQ" --json "$@" 2>/dev/null)
        if [ "$(jq1 "$o" .ok)" = true ]; then jq1 "$o" '.rows[0].ssl'; else echo "ERR: $(jq1 "$o" .error)"; fi; }
err_has() { case "$1" in ERR:*"$2"*) echo yes;; *) echo "no: $1";; esac; }

echo "== certificates (a CA, a server cert for localhost, and an unrelated CA)"
mkdir -p "$W/certs"; cd "$W/certs" || exit 1
cat > ext.cnf <<'EOF'
[srv]
basicConstraints = critical, CA:FALSE
keyUsage = critical, digitalSignature, keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = DNS:localhost
authorityKeyIdentifier = keyid
EOF
mkca() { openssl req -x509 -new -newkey rsa:2048 -nodes -keyout "$1.key" -out "$1.crt" -days 2 -subj "/CN=$2" \
           -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" 2>/dev/null; }
mkca ca "pgbx tls test CA"; mkca other "some other CA"
openssl req -new -newkey rsa:2048 -nodes -keyout server.key -out server.csr -subj "/CN=localhost" 2>/dev/null
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial -out server.crt -days 2 -extfile ext.cnf -extensions srv 2>/dev/null
printf 'local all all trust\nhostssl all all all scram-sha-256\n' > pg_hba.conf
check "server cert chains to the CA" "$(openssl verify -CAfile ca.crt server.crt 2>&1 | tail -1)" "server.crt: OK"
cd "$ROOT" || exit 1

docker rm -fv $PG >/dev/null 2>&1
docker create --rm --name $PG -p "127.0.0.1:$PPORT:5432" -e POSTGRES_PASSWORD="$PGBX_TLS_PW" postgres:16-alpine sh -c '
  mkdir -p /ssl && cp /certs/server.crt /certs/server.key /certs/pg_hba.conf /ssl/ && chown -R postgres /ssl && chmod 600 /ssl/server.key &&
  exec docker-entrypoint.sh postgres -c ssl=on -c ssl_cert_file=/ssl/server.crt -c ssl_key_file=/ssl/server.key -c hba_file=/ssl/pg_hba.conf' >/dev/null
docker cp "$W/certs/." $PG:/certs/ >/dev/null 2>&1
docker start $PG >/dev/null || exit 1
for _ in $(seq 60); do docker logs $PG 2>&1 | grep -q "init process complete" && docker exec $PG pg_isready -q -U postgres 2>/dev/null && break; sleep 1; done
for _ in $(seq 30); do docker exec $PG psql -qAtU postgres -c "SELECT 1" >/dev/null 2>&1 && break; sleep 1; done
# the "cloud" login: a role whose password is the fake tools' token (RDS IAM / Entra stand-in)
docker exec $PG psql -qU postgres -c "CREATE ROLE cloud LOGIN PASSWORD '$TOKEN'" >/dev/null
check "server has ssl on" "$(docker exec $PG psql -qAtU postgres -c 'SHOW ssl')" on

echo "## a. sslmode in the URL"
check "require: TLS"                         "$(ssl --url "$U?sslmode=require")" true
check "prefer: TLS"                          "$(ssl --url "$U?sslmode=prefer")" true
check "allow: TLS (the server only takes TLS)" "$(ssl --url "$U?sslmode=allow")" true
check "no sslmode (default prefer): TLS"     "$(ssl --url "$U")" true
check "disable: refused by hostssl-only pg_hba" "$(err_has "$(ssl --url "$U?sslmode=disable")" "no pg_hba.conf entry")" yes
check "require with channel binding (SCRAM-SHA-256-PLUS)" \
  "$(ssl --url "$U?sslmode=require&channel_binding=require")" true
check "verify-full, unknown CA (default roots): fails" \
  "$(err_has "$(ssl --url "$U?sslmode=verify-full")" "invalid peer certificate")" yes
check "verify-full, the wrong CA file: fails" \
  "$(err_has "$(ssl --url "$U?sslmode=verify-full&sslrootcert=$W/certs/other.crt")" "invalid peer certificate")" yes
check "verify-full + sslrootcert=CA: TLS"   "$(ssl --url "$U?sslmode=verify-full&sslrootcert=$W/certs/ca.crt")" true
check "verify-full by IP (cert names localhost): fails" \
  "$(err_has "$(ssl --url "$U4?sslmode=verify-full&sslrootcert=$W/certs/ca.crt")" "invalid peer certificate")" yes
check "verify-ca by IP: TLS (the name is not checked)" "$(ssl --url "$U4?sslmode=verify-ca&sslrootcert=$W/certs/ca.crt")" true
check "verify-ca, the wrong CA: fails" \
  "$(err_has "$(ssl --url "$U4?sslmode=verify-ca&sslrootcert=$W/certs/other.crt")" "invalid peer certificate")" yes
check "a bad sslmode is a plain error" "$(err_has "$(ssl --url "$U?sslmode=verify_full")" "bad sslmode 'verify_full'")" yes
check "key=value form: sslmode=verify-full sslrootcert=..." \
  "$(ssl --url "host=localhost port=$PPORT user=postgres password=$PGBX_TLS_PW dbname=postgres sslmode=verify-full sslrootcert=$W/certs/ca.crt")" true

echo "## b. PGSSLMODE / PGSSLROOTCERT and precedence"
check "PGSSLMODE=disable: refused" "$(err_has "$(PGSSLMODE=disable ssl --url "$U")" "no pg_hba.conf entry")" yes
check "URL sslmode=require beats PGSSLMODE=disable" "$(PGSSLMODE=disable ssl --url "$U?sslmode=require")" true
check "PGSSLMODE=verify-full + PGSSLROOTCERT: TLS" "$(PGSSLMODE=verify-full PGSSLROOTCERT=$W/certs/ca.crt ssl --url "$U")" true
check "PGSSLMODE=verify-full, no root cert: fails" "$(err_has "$(PGSSLMODE=verify-full ssl --url "$U")" "invalid peer certificate")" yes
check "direct --host/--port honour PGSSLMODE=require" \
  "$(PGSSLMODE=require PGPASSWORD=$PGBX_TLS_PW ssl --host localhost --port $PPORT --user postgres)" true
check "direct --host/--port, PGSSLMODE=disable: refused" \
  "$(err_has "$(PGSSLMODE=disable PGPASSWORD=$PGBX_TLS_PW ssl --host localhost --port $PPORT --user postgres)" "no pg_hba.conf entry")" yes

echo "## c. profiles: url with sslmode=require, profile keys, query + status"
mkdir -p "$PGBX_CONFIG_DIR"
cat > "$PGBX_CONFIG_DIR/config.yaml" <<EOF
default: req
adapters:
  aws: node $ROOT/adapters/aws/aws-adapter.js
  azure: node $ROOT/adapters/azure/azure-adapter.js
profiles:
  req:
    url: postgres://postgres:\$PGBX_TLS_PW@localhost:$PPORT/postgres?sslmode=require
  keys:
    url: postgres://postgres:\$PGBX_TLS_PW@localhost:$PPORT/postgres
    sslmode: verify-full
    sslrootcert: \$PGBX_TLS_CA
  keys_off:
    url: postgres://postgres:\$PGBX_TLS_PW@localhost:$PPORT/postgres
    sslmode: disable
  url_wins:
    url: postgres://postgres:\$PGBX_TLS_PW@localhost:$PPORT/postgres?sslmode=require
    sslmode: disable
  aws_iam:
    adapter: aws
    target: i-0123456789abcdef0
    region: eu-west-1
    user: cloud
    dbname: postgres
    iam_auth: "true"
  azure_entra:
    adapter: azure
    host: localhost
    db_port: $PPORT
    user: cloud
    dbname: postgres
    entra_auth: "true"
  azure_pw:
    adapter: azure
    host: localhost
    db_port: $PPORT
    user: postgres
    password: \$PGBX_TLS_PW
EOF
chmod 600 "$PGBX_CONFIG_DIR/config.yaml"
export PGBX_TLS_CA=$W/certs/ca.crt
out=$("$B" query "$SSLQ" --json); rc=$?
check "query (default profile req): TLS, exit 0, profile_used" "$(jq1 "$out" '.rows[0].ssl')|$rc|$(jq1 "$out" .profile_used)" "true|0|req"
out=$("$B" status --json --profile req); rc=$?
check "status --profile req: ok, exit 0, backups off (stock server)" "$(jq1 "$out" .ok)|$rc|$(jq1 "$out" .backups)" "true|0|off"
"$B" status --profile req >"$W/o" 2>"$W/e"; rc=$?
check "status (text): exit 0, nothing on stderr" "$rc|$(wc -c < "$W/e" | tr -d ' ')" "0|0"
out=$("$B" doctor --json --profile req); rc=$?
check "doctor --profile req: healthy" "$(jq1 "$out" .healthy)|$rc" "true|0"
check "profile keys sslmode: verify-full + sslrootcert: \$VAR" "$(ssl --profile keys)" true
check "profile key sslmode: disable: refused" "$(err_has "$(ssl --profile keys_off)" "no pg_hba.conf entry")" yes
check "the URL's sslmode beats the profile key" "$(ssl --profile url_wins)" true
check "profile key beats PGSSLMODE" "$(PGSSLMODE=require ssl --profile keys_off | cut -c1-4)" "ERR:"
check "nothing secret in the error output" "$(ssl --profile keys_off | grep -c "$PGBX_TLS_PW")" 0

echo "## d. the aws / azure example adapters (their URLs say sslmode=require)"
# a fake aws / az: token commands print the token; aws ssm start-session forwards localPortNumber to the TLS server
cat > "$W/fake-cloud.js" <<EOF
'use strict';
const net = require('node:net');
const a = process.argv.slice(2), has = (...w) => w.every((x) => a.includes(x));
if (has('rds', 'generate-db-auth-token') || has('account', 'get-access-token')) { process.stdout.write('$TOKEN\n'); process.exit(0); }
const p = a.indexOf('--parameters');
if (!has('ssm', 'start-session') || p < 0) { process.stderr.write('fake-cloud: unexpected ' + a.join(' ') + '\n'); process.exit(2); }
const port = Number(/localPortNumber=(\d+)/.exec(a[p + 1])[1]);
net.createServer((c) => { const s = net.connect($PPORT, '127.0.0.1'); c.pipe(s); s.pipe(c); c.on('error', () => s.destroy()); s.on('error', () => c.destroy()); })
  .listen(port, '127.0.0.1');
process.on('SIGTERM', () => process.exit(0));
EOF
printf '#!/bin/sh\nexec node %s "$@"\n' "$W/fake-cloud.js" > "$W/fake-cloud"; chmod +x "$W/fake-cloud"
export PGBX_AWS_BIN=$W/fake-cloud PGBX_AZ_BIN=$W/fake-cloud
out=$("$B" query "SELECT current_user AS u, (SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()) AS ssl" --json --profile aws_iam); rc=$?
check "aws adapter, iam_auth (sslmode=require): TLS as the token's role" "$(jq1 "$out" '.rows[0].u')|$(jq1 "$out" '.rows[0].ssl')|$rc" "cloud|true|0"
out=$("$B" query "SELECT current_user AS u, (SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()) AS ssl" --json --profile azure_entra); rc=$?
check "azure adapter, direct + entra_auth (sslmode=require): TLS" "$(jq1 "$out" '.rows[0].u')|$(jq1 "$out" '.rows[0].ssl')|$rc" "cloud|true|0"
# (status reads shared_preload_libraries, which a plain role like "cloud" may not on a stock server: use postgres)
out=$("$B" status --json --profile azure_pw); rc=$?
check "status through the azure adapter: ok" "$(jq1 "$out" .ok)|$rc" "true|0"
check "the token is not in any output" "$( { "$B" query "SELECT 1/0" --profile aws_iam; "$B" query "SELECT 1/0" --profile azure_entra; } 2>&1 | grep -c "fake-token")" 0

echo "== tls_e2e: $pass passed, $fail failed"
[ $fail -eq 0 ]
