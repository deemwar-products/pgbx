---
title: One database per customer
description: Who pgbx is for, who it is not for, and the problem it solves.
sidebar: { order: 0 }
---

**Every Postgres database you create is backed up to your S3, restore-tested weekly, and controlled from SQL.
No sidecar, no forms.**

## The problem

With a database per tenant, the unit you lose is one tenant's data: a bad migration, a deleted account, a
customer asking for yesterday. Cluster-level backup tools back up and restore the whole cluster: restoring one
database means restoring the whole cluster to a spare machine and copying the database out. The alternative is
hand-written `pg_dump` cron scripts that miss newly created databases.

pgbx works per database:

- **Every new database is backed up.** The extension lives in `template1`, so a database is born with a
  schedule, retention, and a first backup within a minute.
- **Restore one, touch nothing else**: `SELECT pgbx.restore('acme_copy')` restores into a new database while
  every other tenant keeps running; `with_roles` brings the roles it needs ([Roles with every backup](../../guides/roles/)).
- **Policy in migrations**: `pgbx.set_schedule`, `set_retention(gfs => '7d,4w,12m')` and `set_data_scope`
  are SQL, so they ship with the tenant's schema.
- **Proof it works**: weekly restore tests, Prometheus metrics, and alerts to Slack / Telegram / email /
  webhook ([Notifications and metrics](../../guides/notifications/)).
- **Your bucket, your key**: dumps go to your S3 and can be [encrypted](../../guides/encryption/) with AES-256-GCM
  before they leave the server.
- **Agents included**: a CLI that answers in JSON (`pgbx --json`) and an [agent skill](../../agents/skill/), with
  safety tiers; restores only ever write into a new database.

## Use pgbx if

You self-host Postgres 13–18 (VM, bare metal, Docker, or Kubernetes with custom images) and run **many
databases per server**: multi-tenant SaaS with a database per tenant, agencies, platform and internal-tools
teams. It also fits teams that want backup policy in migrations, per-tenant restore without touching other
tenants, or hard evidence that restores actually work.

## Do not use pgbx (alone) if

- you need an **RPO of seconds** for one large database and a mature whole-cluster tool around it: pgbx's optional
  [point-in-time restore](../../guides/point-in-time-restore/) covers the whole server, but tools such as
  pgBackRest, WAL-G or Barman add incremental backups, multiple repositories and standby integration;
- you are on **managed Postgres that blocks extensions** (RDS, Cloud SQL, Azure, Supabase, Neon);
- you have **multi-TB databases** where a logical restore takes too long;
- you need **MySQL or MongoDB** in the same tool.

Next: [install](../../getting-started/install/) · [restore one database](../../guides/restore-database/)
