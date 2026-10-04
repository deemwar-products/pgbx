#!/bin/sh
# keys file from the environment (never printed), then the stock entrypoint with pgbx preloaded
set -eu
mkdir -p /etc/pgbx
printf 'access_key_id=%s\nsecret_access_key=%s\n' "$S3_ACCESS_KEY" "$S3_SECRET_KEY" > /etc/pgbx/s3.credentials
chown postgres /etc/pgbx/s3.credentials; chmod 600 /etc/pgbx/s3.credentials
unset S3_ACCESS_KEY S3_SECRET_KEY
exec docker-entrypoint.sh postgres -c shared_preload_libraries=pgbx -c pgbx.s3_endpoint="$S3_ENDPOINT" \
  -c pgbx.s3_bucket="$BACKUP_S3_BUCKET" -c pgbx.s3_region="$S3_REGION" -c pgbx.server_name="$PGBX_SERVER" -c pgbx.poll_seconds=2
