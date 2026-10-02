# azure adapter

Uses your `az` login. Two ways in:

- **direct**: an Azure Database for PostgreSQL flexible server you can reach from here (`host`); no tunnel;
- **bastion**: Postgres on a VM (or behind a private endpoint a VM can reach), via `az network bastion tunnel`.

| config key | required | default | |
|---|---|---|---|
| `host` | direct | | e.g. `shop.postgres.database.azure.com` |
| `bastion`, `resource_group`, `target_resource_id` | bastion | | |
| `db_port` | | 5432 | |
| `user`, `password`, `dbname` | | | |
| `sslmode` | | `require` | |
| `entra_auth` | | | `"true"`: an Entra ID access token (`az account get-access-token --resource-type oss-rdbms`) as the password |
| `ready_timeout_ms` | | 30000 | |

`PGBX_AZ_BIN` overrides the az binary (tests).
