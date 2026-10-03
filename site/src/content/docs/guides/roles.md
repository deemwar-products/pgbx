---
title: Roles with every backup
description: Each backup keeps the server's roles; restore them with with_roles so owners and grants survive.
sidebar: { order: 14 }
---

`pg_dump` does not include roles. pgbx stores them next to every dump (this is about your application's roles;
the roles pgbx itself creates are in [Concepts › Roles](../../concepts/roles/)):

```
s3://bucket/<server>/<db>/2026-10-01T02-00-00Z.dump
s3://bucket/<server>/<db>/2026-10-01T02-00-00Z.globals.sql.zst   # pg_dumpall --globals-only, zstd (+ encrypted)
```

The roles file also records which roles the database **references**: object owners, ACL grantees,
default-privilege roles and the roles those are members of. Failing to write it never fails the backup
(`params.globals_error` says why).

Passwords are left out by default (`--no-role-passwords`). Set `pgbx.backup_role_passwords = on` to keep the
password hashes (turn on [encryption](../encryption/) first: the hashes then sit in your bucket).

## Restore with roles

```sql
SELECT pgbx.restore('shop_copy', with_roles => true);                 -- roles shop uses
SELECT pgbx.restore('shop_copy', with_roles => true, roles => 'all');  -- every role of the old server
```

```sh
pgbx db-restore --db shop --into shop_copy --with-roles --wait
pgbx db-restore --from-s3 --db shop --into shop --with-roles [--roles all] [--key-file K] <s3 flags>
```

Rules, so it is safe to run on a shared server and to run twice:

- a role that does **not** exist is created (`CREATE ROLE`, its attributes, comments, settings, and its
  memberships);
- a role that **exists** is left exactly as it is (no `ALTER ROLE`, no new memberships) and reported;
- `roles => 'referenced'` (default) only creates roles this database uses: another tenant's roles stay out;
- tablespaces are skipped (paths are server-specific); create them by hand if needed;
- with roles, object owners are kept (`pg_restore` without `--no-owner`); without, as before, everything is
  owned by the restoring role.

The job's `params.roles` (or the CLI's `roles`) report `created`, `existing`, `out_of_scope`, `skipped`,
`failed`. Roles created without a password need one before they can log in: `ALTER ROLE app PASSWORD '...'`.
Backups taken before 0.6 have no roles file; restore them without `with_roles`.
