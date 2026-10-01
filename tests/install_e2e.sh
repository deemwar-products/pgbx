#!/bin/sh
# install.sh + pgbx setup end to end in a throwaway debian:bookworm container with postgresql-16 from apt.
#   tests/install_e2e.sh DIST_DIR
# DIST_DIR holds locally built release assets for this machine's arch:
#   pgbx-linux-<arch>.tar.gz, pgbx-ext-pg16-linux-<arch>.tar.gz, SHA256SUMS
# Checks: files land in pg_config dirs, checksum mismatch aborts, pgbx runs, the skill prompt (no tty -> yes,
# --no-skill -> no, tty answer n -> no), `pgbx setup --yes` merges shared_preload_libraries into conf.d/pgbx.conf
# and writes a 0600 postgres-owned credentials file, and Postgres restarts with it.
set -eu
DIST=$(CDPATH= cd -- "${1:?usage: tests/install_e2e.sh DIST_DIR}" && pwd -P)
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
C=pgbx-inst-e2e
docker rm -f "$C" >/dev/null 2>&1 || true
trap 'docker rm -f "$C" >/dev/null 2>&1 || true' EXIT
docker run -d --platform "linux/${ARCH:-$(uname -m | sed "s/x86_64/amd64/;s/aarch64/arm64/")}" --name "$C" -v "$DIST:/dist:ro" -v "$ROOT/site/public/install.sh:/install.sh:ro" debian:bookworm sleep infinity >/dev/null
x() { docker exec "$C" sh -c "$1"; }
pass() { echo "PASS: $*"; }
fail() { echo "FAIL: $*"; exit 1; }

# bookworm ships PostgreSQL 15; 16 comes from the PGDG apt repo
x 'export DEBIAN_FRONTEND=noninteractive; apt-get update -qq && apt-get install -y -qq postgresql-common curl ca-certificates sudo bsdutils >/dev/null 2>&1 \
   && /usr/share/postgresql-common/pgdg/apt.postgresql.org.sh -y >/dev/null 2>&1 && apt-get install -y -qq postgresql-16 >/dev/null 2>&1'
x 'useradd -m -s /bin/sh alice && echo "alice ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/alice'
x "sed -i \"s/^#shared_preload_libraries = ''/shared_preload_libraries = 'pg_stat_statements'/\" /etc/postgresql/16/main/postgresql.conf"
x 'pg_ctlcluster 16 main start'

# 1. checksum mismatch aborts before anything is installed
x 'mkdir /bad && cp /dist/SHA256SUMS /dist/*.tar.gz /bad/ && sed -i "s/^......../deadbeef/" /bad/SHA256SUMS'
if x 'sh /install.sh --base-url file:///bad --no-skill --prefix /opt/bad' >/tmp/pgbx-inst-bad.log 2>&1; then fail "mismatch accepted"; fi
grep -q "checksum mismatch" /tmp/pgbx-inst-bad.log && x '! test -e /opt/bad/pgbx' && pass "checksum mismatch aborts, nothing installed"

# 2. root install: CLI + extension, skill skipped for root
x 'sh /install.sh --base-url file:///dist' | tee /tmp/pgbx-inst-root.log
x 'pgbx --version' && pass "pgbx runs"
x 'test -f /usr/lib/postgresql/16/lib/pgbx.so && test -f /usr/share/postgresql/16/extension/pgbx.control && ls /usr/share/postgresql/16/extension/ | grep -q "^pgbx--.*\.sql$"' && pass "extension files in pkglibdir/sharedir"
grep -q "skipped as root" /tmp/pgbx-inst-root.log && pass "skill skipped for root"
grep -q "sudo pgbx setup" /tmp/pgbx-inst-root.log && pass "prints next step"

# 3. skill prompt as a normal user
x 'su - alice -c "sh /install.sh --base-url file:///dist --no-extension --prefix /home/alice/.local/bin" </dev/null'
x 'test -L /home/alice/.claude/skills/pgbx-skill' && pass "no tty -> skill installed"
x 'su - alice -c "/home/alice/.local/bin/pgbx skill uninstall >/dev/null"'
x 'su - alice -c "sh /install.sh --base-url file:///dist --no-extension --prefix /home/alice/.local/bin --no-skill" </dev/null'
x '! ls /home/alice/.claude/skills/*skill* 2>/dev/null' && pass "--no-skill -> not installed"
x "su - alice -c \"printf 'n\\n' | script -qec 'sh /install.sh --base-url file:///dist --no-extension --prefix /home/alice/.local/bin' /dev/null\"" | tee /tmp/pgbx-inst-tty.log
grep -q "Install the pgbx agent skill" /tmp/pgbx-inst-tty.log && x '! ls /home/alice/.claude/skills/*skill* 2>/dev/null' && pass "tty answer n -> not installed"

# 4. pgbx setup: plan without --yes, then apply
x 'pgbx setup --json --s3-endpoint https://s3.example.com --s3-bucket my-backups --s3-region eu-1 --server-name db1 --credentials-file /etc/pgbx/s3.credentials' | tee /tmp/pgbx-inst-plan.json
grep -q '"applied":false' /tmp/pgbx-inst-plan.json && x '! test -e /etc/postgresql/16/main/conf.d/pgbx.conf' && pass "setup without --yes writes nothing"
x 'FAKE_AK=AKIAFAKEFAKE FAKE_SK=fakesecretfake pgbx setup --yes --json --s3-endpoint https://s3.example.com --s3-bucket my-backups --s3-region eu-1 --server-name db1 --access-key-env FAKE_AK --secret-key-env FAKE_SK' | tee /tmp/pgbx-inst-setup.json
grep -q '"applied":true' /tmp/pgbx-inst-setup.json || fail "setup did not apply"
grep -q 'fakesecret' /tmp/pgbx-inst-setup.json && fail "secret leaked into output"
x "grep -qx \"shared_preload_libraries = 'pg_stat_statements,pgbx'\" /etc/postgresql/16/main/conf.d/pgbx.conf" && pass "conf.d/pgbx.conf merges shared_preload_libraries"
x 'grep -q "pgbx.s3_bucket = '"'"'my-backups'"'"'" /etc/postgresql/16/main/conf.d/pgbx.conf && ! grep -q archive_mode /etc/postgresql/16/main/conf.d/pgbx.conf' && pass "pgbx.* settings, no archive_mode"
[ "$(x 'stat -c "%a %U" /etc/pgbx/s3.credentials')" = "600 postgres" ] && pass "credentials 0600 owned by postgres"
grep -q 'systemctl restart postgresql@16-main' /tmp/pgbx-inst-setup.json && pass "prints the restart command"
# restart with the new conf
x 'pg_ctlcluster 16 main restart'
x "su postgres -c \"psql -Atc 'SHOW shared_preload_libraries'\"" | grep -qx 'pg_stat_statements,pgbx' && pass "Postgres restarted with merged shared_preload_libraries"
# idempotent re-run keeps one pgbx
x 'FAKE_AK=a FAKE_SK=b pgbx setup --yes --json --s3-endpoint https://s3.example.com --s3-bucket my-backups --access-key-env FAKE_AK --secret-key-env FAKE_SK >/dev/null'
x "grep -qx \"shared_preload_libraries = 'pg_stat_statements,pgbx'\" /etc/postgresql/16/main/conf.d/pgbx.conf" && pass "re-run stays merged (no duplicate)"
echo "ALL PASSED"
