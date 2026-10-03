# AWS Marketplace AMI: spec for the marketplace agent

Free AMI: self-managed PostgreSQL on EC2 with pgbx backups built in. MIT; paid: installation, support, training
(io@deemwar.com). Status: **spec, not built.** Owner decisions are in docs/adr/0003 (marketplace section).

## 1. Postgres

- The pgbx extension supports **PostgreSQL 13–18** (amd64 and arm64). Ship **PostgreSQL 17** by default (18 is
  newest but younger); one AMI per arch (x86_64 and arm64/Graviton).
- **Ubuntu 24.04 LTS + PGDG apt** (`apt.postgresql.org`), not Amazon Linux 2023. Release artifacts are built on
  Ubuntu 22.04 (glibc 2.35) and load on Debian 12 / Ubuntu 22.04+; AL2023 has no PGDG apt and would need RPM
  packaging we don't have or test.

## 2. Install (in the Packer build)

```sh
apt-get install -y postgresql-17            # from PGDG
curl -fsSL https://deemwar-products.github.io/pgbx/install.sh | sh -s -- --version v0.6.0 --pg-version 17 --no-skill
```

- `install.sh` checks every file against the release's SHA256SUMS and installs the pgbx CLI to /usr/local/bin and
  the extension files for every local Postgres. It **never edits config or restarts anything**. Pin `--version`;
  **v0.6.0 must be released first** (today only v0.5.0 is; the owner runs the manual Release workflow).
- No system deps beyond Postgres itself (`pg_dump` / `pg_restore` come with it) and `curl` + `sha256sum` for the
  installer. The CLI is a static binary.
- Config: `shared_preload_libraries = 'pgbx'` (one restart), plus `pgbx.s3_endpoint`, `pgbx.s3_bucket`,
  `pgbx.s3_region`, `pgbx.server_name` and `pgbx.credentials_file`. `sudo pgbx setup server --yes` writes all of
  them into `conf.d/pgbx.conf` and prints the restart command. **No `CREATE EXTENSION` is needed:** the pgbx
  worker creates the extension in `template1` and every database by itself (name: `pgbx`, schema `pgbx`).
- Optional point-in-time restore: `sudo pgbx setup pitr --yes` (sets `archive_mode`/`archive_command`; another
  restart). Leave it off by default; document it.

## 3. First boot (per instance)

- Generate the `postgres` password into a root-only file (e.g. `/root/pgbx-ami/postgres-password`, 0600); no
  default password anywhere in the image.
- `listen_addresses = '*'` with `pg_hba.conf` allowing only the VPC CIDR (from IMDSv2), `scram-sha-256`; TLS on
  with a self-signed cert generated at first boot (`ssl = on`). Clients connect with `sslmode=require`; pgbx's CLI
  supports it.
- **pgbx-specific:** `pgbx.server_name` = the instance id (IMDSv2), so each instance gets its own folder in the
  bucket. S3 bucket/region come from CloudFormation parameters (or EC2 user data), written by
  `pgbx setup server --yes`.
- **S3 credentials: BLOCKER.** pgbx today reads static keys from `pgbx.credentials_file` only; it cannot yet use
  the EC2 instance role (IMDSv2). Static keys in a marketplace AMI are poor practice. **Required before listing:**
  pgbx gains instance-role credentials (e.g. `pgbx.credentials_file = 'instance-role'`, or used automatically when
  no file is set, via IMDSv2). The pgbx session will build this; the CloudFormation template then attaches an
  instance profile with `s3:PutObject/GetObject/ListBucket/DeleteObject/AbortMultipartUpload` on that bucket only.
- Optional: encryption at rest for dumps (`pgbx.key_file`, a raw 32-byte key generated at first boot, root/postgres
  only). Tell the customer to back up that key; without it the dumps can't be restored.

## 4. Smoke test (Packer build and launch test)

With S3 configured:

```sql
-- in the postgres database, as postgres
SELECT name, ok, detail FROM pgbx.doctor() WHERE NOT ok;      -- expect 0 rows (advice rows may warn)
CREATE DATABASE ami_smoke;
\c ami_smoke
SELECT * FROM pgbx.status();                                  -- state shows 'waiting for first backup' or 'ok'
SELECT pgbx.backup_now();                                     -- returns a job id; poll:
SELECT state, error FROM pgbx.history WHERE kind = 'backup' ORDER BY id DESC LIMIT 1;   -- 'done' within ~1 min
```

Without S3 (Packer build, no bucket yet): `SELECT extversion FROM pg_extension WHERE extname = 'pgbx'` returns
`0.6.0` in `template1`, and `pgbx --version` prints `pgbx 0.6.0`. `pgbx doctor --json` reports only the
"s3 settings" check as not ok.

## 5. Listing text

- Docs: **https://deemwar-products.github.io/pgbx/** (no muthuishere links anywhere).
- Description:
  > PostgreSQL with backups that look after themselves. This AMI runs PostgreSQL 17 with pgbx, an open-source (MIT)
  > extension that backs up every database to your own S3 bucket automatically, including databases you create
  > later, and restores any of them with one command into a new database, even onto a new server. Backups are plain
  > pg_dump files you own: portable, restorable anywhere, kept on your schedule and retention. Optional
  > point-in-time restore and encryption. The software is free; Deemwar offers paid installation, support and
  > training (io@deemwar.com).

## 6. The AMI must NOT include

- Any S3 keys, passwords, encryption keys or certificates (all generated or provided at first boot).
- Telemetry or phone-home of any kind (pgbx has none; don't add any).
- The agent skill (`--no-skill`), memory files (`~/pgbx/`) or CLI profiles.
- The future "runner" mode (for RDS/Cloud SQL): not built; a separate container listing later.
- The `/metrics` endpoint or `pgbx serve` exposed to the network (both local-only by default; keep it that way).
