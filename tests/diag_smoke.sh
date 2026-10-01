#!/bin/sh
# Throwaway smoke test for `pgbx doctor` / `pgbx diagnose` with pg_wal piling up (runs INSIDE the pgbx-dev image):
#   docker run --rm --name pgbx-diag-smoke -v "$PWD":/src -v pgbx-diag-target:/src/target \
#     -v pgbx-diag-clitarget:/src/cli/target -w /src pgbx-dev sh tests/diag_smoke.sh
# No S3 needed: a failing archive_command of 'another tool' (/bin/false) and a dead replication slot make WAL pile up.
set -eu
cargo pgrx package --pg-config /usr/lib/postgresql/16/bin/pg_config >/dev/null 2>&1
cp target/release/pgbx-pg16/usr/lib/postgresql/16/lib/pgbx.so /usr/lib/postgresql/16/lib/
cp target/release/pgbx-pg16/usr/share/postgresql/16/extension/* /usr/share/postgresql/16/extension/
(cd cli && cargo build --release -q) && cp cli/target/release/pgbx /usr/local/bin/pgbx
B=/usr/lib/postgresql/16/bin; D=/tmp/pgd; W=/tmp/pgbxwd
mkdir -p $W /var/run/postgresql && chown postgres $W /var/run/postgresql
su postgres -c "$B/initdb -D $D -A trust >/dev/null"
cat >> $D/postgresql.conf <<CONF
shared_preload_libraries = 'pgbx'
archive_mode = on
archive_command = '/bin/false'
wal_level = replica
logging_collector = on
log_directory = 'log'
pgbx.poll_seconds = 2
pgbx.alert_command = 'cat >> $W/alerts.log; echo >> $W/alerts.log'
CONF
su postgres -c "$B/pg_ctl -D $D -w start >/dev/null"
su postgres -c "psql -XAtq -c \"CREATE TABLE t(x int)\""
P() { su postgres -c "psql -XAtq -c \"$1\""; }
sleep 3
P "SELECT pg_create_physical_replication_slot('dead_consumer', true)" >/dev/null
for i in $(seq 8); do P "INSERT INTO t SELECT generate_series(1,100000); SELECT pg_switch_wal()" >/dev/null; done
sleep 5
echo "--- doctor() rows"
P "SELECT name, ok, detail FROM pgbx.doctor() WHERE name IN ('archive_mode','replication_slots')"
test "$(P "SELECT count(*) FROM pgbx.doctor() WHERE name ~ '^wal'")" = 0 || { echo "FAIL: WAL-archiving rows still in doctor()"; exit 1; }
test "$(P "SHOW archive_command")" = "/bin/false" || { echo "FAIL: the worker touched an archive_command it did not set"; exit 1; }
echo "--- pgbx doctor (text, slot lines)"
su postgres -c "pgbx doctor --pgdata $D" | grep -E -A1 "archive_mode|replication" || true
echo "--- stop postgres, fake an OOM line, pgbx diagnose --json"
su postgres -c "$B/pg_ctl -D $D -m fast stop >/dev/null"
printf '%s\n' "2026-10-01 09:00:00 UTC LOG:  checkpoint starting" \
  "[ 4242.1] Out of memory: Killed process 4242 (postgres) total-vm:9000000kB, anon-rss:8000000kB" > /tmp/fake.log
su postgres -c "pgbx diagnose --json --log /tmp/fake.log --pgdata $D" > /tmp/diag.json || true
python3 -c "import json;d=json.load(open('/tmp/diag.json'));print(d['postgres'],d['probable_cause']);[print(' ',s['tier'],'|',s['command'][:90]) for s in d['steps']];print(' ready_wal',d['facts']['ready_wal'])" 2>/dev/null || cat /tmp/diag.json
echo "--- pgbx doctor with postgres down (text tail)"
su postgres -c "pgbx doctor --log /tmp/fake.log --pgdata $D" | head -12 || true
echo "--- stale pid case"
echo 999999 > $D/postmaster.pid; chown postgres $D/postmaster.pid
su postgres -c "pgbx diagnose --json --pgdata $D" | python3 -c "import json,sys;d=json.load(sys.stdin);print(d['postgres'],d['probable_cause'])"
