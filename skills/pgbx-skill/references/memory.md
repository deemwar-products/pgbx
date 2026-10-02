# Memory — recipes

Tier: read-only to read; writing a memory file is a local, user-requested edit (never automatic).

Every database has its own memory, as plain Markdown files on the user's machine:

```
${PGBX_MEMORY_DIR:-~/pgbx}/<connection>/<db>/memories.md   # facts, named questions + their SQL, conventions
${PGBX_MEMORY_DIR:-~/pgbx}/<connection>/<db>/tables.md     # tables, columns, what they mean
```

- `<connection>` = the pgbx profile name (`--profile prod` → `prod`); without a profile, the host (`localhost`).
- `<db>` = the database name. One folder per database: never mix two databases in one file.
- `PGBX_MEMORY_DIR` moves the root; `PGBX_MEMORY=off` turns memory off (don't read, don't write).
- The files belong to the user. **Never create a folder or file unless the user asks** ("remember this",
  "put it here", "save this query", "note the tables"). Never store row data, credentials, S3 keys or
  download links: only schema, SQL text and the user's own notes.

**User-visible formatting (family default):** say which file you read or wrote, by path, in one line.

---

### MEM-R-1: Read memory before working on a database

**When to use:** always, before writing a `pgbx query` or acting on a database, once the profile and
database are known (step-00). Also for "what do you remember about <db>".

**Command:**
```bash
D="${PGBX_MEMORY_DIR:-$HOME/pgbx}/<connection>/<db>"
[ "${PGBX_MEMORY:-on}" = off ] || { cat "$D/memories.md" "$D/tables.md" 2>/dev/null; }
```

**Expected response:** the two files' contents, or nothing when they do not exist (that is normal; do not
create them). Use what is there: table and column names from `tables.md`, a saved question's SQL from
`memories.md` (reuse it as written instead of inventing a new query).

**Common errors:** missing files → continue without memory; never guess a table name, look it up with
STAT-R-7 (`information_schema`) instead. A saved query that now fails (column renamed) → tell the user and
offer to update the memory, don't silently change it.

**User-visible formatting:** "Using memory from ~/pgbx/prod/shop/ (3 notes, 12 tables)." or nothing when
there is none.

---

### MEM-W-1: Remember a fact or a question

**When to use:** the user says "remember …", "put it here", "save this query as …", "note that …".

**Command:**
```bash
D="${PGBX_MEMORY_DIR:-$HOME/pgbx}/<connection>/<db>"; mkdir -p "$D"
cat >> "$D/memories.md" <<'EOF'

## orders today
Orders placed in the last 24 h. `created_at` is UTC.
```sql
SELECT count(*) FROM orders WHERE created_at > now() - interval '1 day'
```
EOF
```

**Expected response:** the file now ends with the new section. Append; never rewrite or reorder what the
user wrote. A named question = `## <name>`, one line of meaning, then the exact SQL that worked.

**Common errors:** `PGBX_MEMORY=off` → say memory is off and write nothing. The user asks to store a
password, key or row data → refuse that part and store only the rest.

**User-visible formatting:** "Saved 'orders today' to ~/pgbx/prod/shop/memories.md."

---

### MEM-W-2: Note the tables

**When to use:** "note the tables", "remember the schema", "put the tables here".

**Call sequence:**
```bash
pgbx query --profile <connection> --db <db> --json \
  "SELECT table_schema, table_name, column_name, data_type FROM information_schema.columns
    WHERE table_schema NOT IN ('pg_catalog','information_schema','pgbx') ORDER BY 1,2,ordinal_position"
```
Then write `tables.md` (create it only now, since the user asked): one `## schema.table` section per table,
a bullet per column `name type`. Keep any lines the user already wrote; add new tables, mark dropped ones
`(gone)` instead of deleting them.

**Expected response:** `tables.md` lists every user table with its columns.

**Common errors:** `truncated: true` in the query result → re-run per schema; a very large schema → ask
which tables matter and note only those.

**User-visible formatting:** "Noted 12 tables in ~/pgbx/prod/shop/tables.md."

---

### MEM-W-3: Forget

**When to use:** "forget …", "remove that note", "clear the memory for <db>".

**Command:** edit the file and delete only the named `## <name>` section. For "clear the memory", ask once,
then:
```bash
rm "${PGBX_MEMORY_DIR:-$HOME/pgbx}/<connection>/<db>/memories.md"   # tables.md too only if the user said so
```

**Expected response:** the section or file is gone; nothing else changed.

**Common errors:** the name matches more than one section → list them and ask which.

**User-visible formatting:** "Removed 'orders today' from ~/pgbx/prod/shop/memories.md."
