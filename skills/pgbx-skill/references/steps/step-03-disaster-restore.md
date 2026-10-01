# Step 03 — Server lost: restore databases on a new server

Tier: **safe mutation** (every restore goes into a NEW database). Works without pgbx on the target.

1. **Facts (read-only):** which databases matter, the old `server_name`, the bucket/endpoint/region and a
   credentials file path (never read or print it). RST-R-3 lists the dumps per database.
2. **Confirm the point:** newest dump, or the newest at or before a time the user names (with a UTC offset).
3. **Restore each database:** RST-R-4 into a NEW name. It refuses a database that already exists.
4. **After:** report per database the dump key time and the new name. Renaming it to the live name or
   dropping anything is RST-R-2 (destructive, explicit approval).

Old whole-server (pgBackRest) data from before 0.5.0 under `<server>/<system id>/_cluster/` is not used;
deleting it is the human's call.
