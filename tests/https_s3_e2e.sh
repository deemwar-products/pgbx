#!/usr/bin/env bash
# Real HTTPS to S3 (the 0.6.0 bug every other suite missed: they all use plain-http local S3). With the build's two
# rustls providers (aws-lc-rs + ring) and none chosen, the first https request panicked
# ("Could not automatically determine the process-level CryptoProvider"), so no backup reached a real S3.
# This talks to real AWS S3 over https with DUMMY keys: AWS answering "InvalidAccessKeyId"/403 proves the TLS
# handshake and the signed request worked; a panic or a CryptoProvider message fails. Needs internet; no account.
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
IMG=${PGBX_IMAGE:-pgbx:test}
B=${PGBX_BIN:-$ROOT/cli/target/debug/pgbx}
C=pgbx-https-s3
EP=https://s3.us-east-1.amazonaws.com
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "  PASS $1"; pass=$((pass+1)); else echo "  FAIL $1 (got '$2', want '$3')"; fail=$((fail+1)); fi; }
W=$(mktemp -d); trap 'docker rm -f $C >/dev/null 2>&1; rm -rf "$W"' EXIT
printf 'access_key_id=AKIAPGBXTESTNOTREAL00\nsecret_access_key=pgbx-test-not-a-real-secret-0000000000000\n' > "$W/creds"; chmod 644 "$W/creds"
P() { docker exec -u postgres $C psql -qAt "$@"; }

echo "## 1. extension: a backup job over https to real S3 (dummy keys)"
docker rm -f $C >/dev/null 2>&1
docker run -d --rm --name $C -e POSTGRES_PASSWORD=test-only-not-secret -v "$W/creds":/etc/pgbx/s3.credentials:ro "$IMG" postgres \
  -c shared_preload_libraries=pgbx -c pgbx.s3_endpoint=$EP -c pgbx.s3_bucket=pgbx-https-test-nonexistent-bucket \
  -c pgbx.s3_region=us-east-1 -c pgbx.server_name=https-test -c pgbx.credentials_file=/etc/pgbx/s3.credentials \
  -c pgbx.poll_seconds=2 >/dev/null || { echo "cannot start $IMG"; exit 1; }
for _ in $(seq 120); do docker logs $C 2>&1 | grep -q "database system is ready to accept connections" && P -c "SELECT 1" >/dev/null 2>&1 && break; sleep 1; done
for _ in $(seq 60); do [ "$(P -c "SELECT count(*) FROM pg_extension WHERE extname='pgbx'" 2>/dev/null)" = 1 ] && break; sleep 1; done
id=$(P -c "SELECT pgbx.backup_now()")
st=""; for _ in $(seq 200); do st=$(P -c "SELECT state FROM pgbx.history WHERE id=$id"); case "$st" in done|failed) break;; esac; sleep 2; done
err=$(P -c "SELECT coalesce(error,'') FROM pgbx.history WHERE id=$id")
echo "  job: $st: ${err:0:160}"
check "job finished (failed on the dummy keys, as expected)" "$st" failed
check "no panic / CryptoProvider error" "$(echo "$err" | grep -ciE 'panick|CryptoProvider')" 0
check "AWS answered over TLS (InvalidAccessKeyId / 403 / AccessDenied)" "$(echo "$err" | grep -ciE 'InvalidAccessKeyId|403|AccessDenied|SignatureDoesNotMatch' | head -c1)" 1
check "no panic in the worker log" "$(docker logs $C 2>&1 | grep -ciE 'panicked|CryptoProvider')" 0

echo "## 2. CLI: --from-s3 over https (dummy keys)"
[ -x "$B" ] || cargo build -q --manifest-path "$ROOT/cli/Cargo.toml" || exit 1
out=$("$B" backups --from-s3 --db shop --s3-endpoint $EP --s3-bucket pgbx-https-test-nonexistent-bucket --s3-region us-east-1 \
       --server-name https-test --credentials-file "$W/creds" --json 2>&1)
check "CLI: no panic / CryptoProvider error" "$(echo "$out" | grep -ciE 'panick|CryptoProvider')" 0
check "CLI: AWS answered over TLS (its XML error came back)" "$(echo "$out" | grep -ciE 'InvalidAccessKeyId|403|AccessDenied|SignatureDoesNotMatch|serde xml' | head -c1)" 1

echo "== https_s3_e2e: $pass passed, $fail failed"
[ $fail -eq 0 ]
