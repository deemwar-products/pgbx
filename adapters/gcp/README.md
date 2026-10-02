# gcp adapter

Reaches **Cloud SQL for PostgreSQL** through the Cloud SQL Auth Proxy (v2), with your gcloud / Application
Default Credentials.

| config key | required | default | |
|---|---|---|---|
| `instance` | yes | | `project:region:instance` |
| `iam_auth` | | | `"true"`: `--auto-iam-authn` (the `user` is then your IAM principal) |
| `private_ip` | | | `"true"`: `--private-ip` |
| `user`, `password`, `dbname` | | | |
| `ready_timeout_ms` | | 30000 | |

Runs `cloud-sql-proxy <instance> --port <free> --address 127.0.0.1`. The proxy encrypts to the instance, so the
local hop is `sslmode=disable`. `PGBX_CLOUD_SQL_PROXY_BIN` overrides the binary (tests).
