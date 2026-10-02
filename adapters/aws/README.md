# aws adapter

Reaches RDS / Aurora / EC2 Postgres through **AWS Systems Manager port forwarding**, with no open ports and no
bastion host, using your own `aws` CLI credentials. Needs the AWS CLI v2 and the session-manager-plugin.

| config key | required | default | |
|---|---|---|---|
| `target` | yes | | an SSM-managed instance id (`i-...`) that can reach the database |
| `db_host` | | `localhost` | the RDS endpoint as seen from that instance; `localhost` = Postgres on the instance itself |
| `db_port` | | 5432 | |
| `region`, `aws_profile` | | | passed as `--region` / `--profile` |
| `user`, `password`, `dbname`, `sslmode` | | | |
| `iam_auth` | | | `"true"`: mint an RDS IAM auth token (`aws rds generate-db-auth-token`) as the password; `sslmode` becomes `require` |
| `ready_timeout_ms` | | 30000 | |

Uses `AWS-StartPortForwardingSessionToRemoteHost` (or `AWS-StartPortForwardingSession` for `localhost`).
The IAM token is used once to connect and never printed. `PGBX_AWS_BIN` overrides the aws binary (tests).
