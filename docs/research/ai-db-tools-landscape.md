# AI and agent tools for Postgres: the landscape, and where pgbx stands (research, 2026-10-02)

Research only. Nothing here has been posted, sent, or changed in the code or site. The comparison covers products only
(features, workflow, depth, safety, complaints).

**Sources.**
- **pgbx facts** come from this repo at `71f1314`: `README.md`, `site/src/content/docs/` (concepts/how-it-works,
  concepts/safety-tiers, agents/skill, reference/cli, guides/postgres-down), `docs/adr/0001-load-aware-backups.md`,
  `docs/adr/0002-cli-profiles-ssh-and-read-queries.md`, `docs/marketing/positioning.md`, `cli/src/query.rs`.
- **Market facts** come from web research done on 2026-10-02 (URLs inline). I re-checked three of them by hand
  (marked ✔): the DBHub CVE, the AWS Labs CVEs, and the SimpleBackups MCP.
- Anything not confirmed is marked **UNVERIFIED**.

---

## 0. pgbx today, in one paragraph

pgbx has three parts:
1. **The extension** runs inside Postgres 13–18: per-database `pg_dump` to S3, schedules, retention, weekly verify
   restores, restores only into a NEW database, `doctor()`, `overview()`, a server-wide job queue with coalescing and
   cancel, and resource caps (ADR 0001).
2. **The CLI** (`pgbx`, Linux, macOS arm64, Windows) does the following:
   - named **profiles**: locations only, never secrets
   - **SSH through the system `ssh`**, so `~/.ssh/config`, the ssh agent, and `--ssh-jump`/ProxyJump all work;
     a helper keeps the tunnel open and reuses it, closing it after 10 min unused
   - `pgbx query`, which accepts SELECT-style statements only and returns typed JSON. Its guard has four layers: a
     single statement, a start-keyword allowlist, a deny-list of side-effect functions, and
     `BEGIN READ ONLY` + timeouts + ROLLBACK
   - `diagnose` and `doctor`, which work **when Postgres is down** (logs, OOM, disk, WAL, replication slots, pid)
     and suggest tiered steps that pgbx never runs itself
   - `db-restore --from-s3` onto a bare server
   - `setup server|client`
   - `memories export|import|path`
   - `--json` on every command, which prints one object with `ok`, `command` and `safety`
3. **The agent skill** (Claude Code and Codex) drives only the CLI, never ssh or psql:
   - It reads the per-database memory in `~/pgbx/<connection>/<db>/memories.md` and `tables.md` before acting.
   - It writes that memory only when the user asks.
   - Safety tiers (readonly / safe / guarded / destructive) are enforced in code, and `--yes` counts as the
     human's signature.

**Client-only use.** Someone can use pgbx as a day-to-day client with no backups: `profile`, `query`, `tunnel` and
`memories` work against **plain Postgres with no extension** (`tests/ssh_e2e.sh` runs on a plain postgres). Some
parts do need more:
- `doctor`, `logs` and `diagnose` over SSH need the pgbx CLI on the host.
- `setup client` reports `ok:false` when the extension is missing. That is a friction point for client-only users
  (see §3).

---

## 1. Capability matrix

Key: ✅ yes · 🟡 partial · ❌ no · ? = UNVERIFIED.

| tool | SSH / jump host | profiles | read-only guard / safety tiers | JSON for agents | per-DB memory / notes | backups / restore | diagnose when PG is down | no server components | local-first, no account | agent skill / MCP | license / price |
|---|---|---|---|---|---|---|---|---|---|---|---|
| **pgbx** | ✅ system ssh, `~/.ssh/config`, jump host, tunnel reused for 10 min | ✅ | ✅ 4 layers (1 stmt, allowlist, fn deny-list, READ ONLY txn) + 4 tiers for every command | ✅ one object per command (`ok/command/safety`) | ✅ Markdown files per DB, export/import | ✅ extension: S3, retention, verify, restore to a new DB, restore from S3 onto a bare server | ✅ `diagnose` (logs, OOM, disk, WAL, slots) | 🟡 query: none; backups need the extension; diagnose needs the CLI on the host | ✅ | ✅ skill (Claude Code/Codex); ❌ MCP | MIT, free |
| Reference `server-postgres` (MCP) | ❌ | ❌ 1 URL | 🟡 READ ONLY txn only, **bypassed** by `COMMIT; DROP …` ([Datadog](https://securitylabs.datadoghq.com/articles/mcp-vulnerability-case-study-SQL-injection-in-the-postgresql-mcp-server/)) | 🟡 | 🟡 schema as resources | ❌ | ❌ | ✅ | ✅ | MCP | MIT; **archived 2025-05-29** |
| Crystal DBA postgres-mcp (Pro) | ❌ ? | ❌ | ✅ restricted mode: READ ONLY + pglast parser + timeout | 🟡 | ❌ | ❌ | ❌ | 🟡 needs pg_stat_statements + hypopg for tuning | ✅ | MCP | MIT; last commit Jan 2026, seen as stalled ([issues](https://github.com/crystaldba/postgres-mcp/issues)) |
| DBHub (Bytebase) | ✅ key-based ssh + multi-hop `ssh_proxy_jump`; no `~/.ssh/config` | ✅ TOML `[[sources]]` | 🟡 keyword classifier; **CVE-2026-61788** ✔: read-only did nothing until 0.22.6 ([GHSA](https://github.com/advisories/GHSA-mwwr-p57h-56pf)) | 🟡 | 🟡 custom tools in TOML | ❌ | ❌ | ✅ | ✅ | MCP + web workbench | MIT; multi-DB |
| Google MCP Toolbox for Databases | ❌ ? | ✅ `tools.yaml` | ❌ none on `postgres-execute-sql`; safety = write narrow tools | 🟡 | 🟡 hand-written tools | ❌ | ❌ | ✅ | ✅ | MCP (+ OTel, IAM auth) | Apache-2.0 ([repo](https://github.com/googleapis/genai-toolbox)) |
| AWS Labs postgres-mcp-server | ❌ (Data API / IAM) | 🟡 | 🟡 pglast + denylist; **CVE-2026-85787** ✔ `set_config()` bypass and **CVE-2026-87911** ✔ `COPY TO PROGRAM` RCE, fixed in 1.1.7 ([AWS bulletin](https://aws.amazon.com/security/security-bulletins/2026-104-aws/)) | 🟡 | ❌ | ❌ | ❌ | 🟡 IAM / Secrets Manager | ❌ AWS | MCP | Apache-2.0 ? |
| pgEdge Postgres MCP | ❌ ? | ✅ | ✅ READ ONLY + rejects `DO` / `set_config` escapes | 🟡 | ❌ | ❌ | ❌ | ✅ | ✅ | MCP + NL CLI + web UI | PostgreSQL lic. ([GA post](https://www.pgedge.com/blog/pgedge-mcp-server-for-postgres-is-now-ga-here-s-why-that-matters)) |
| Supabase MCP | ❌ (cloud) | 🟡 project_ref | ✅ `read_only` = restricted PG role; prompt-injection wrapping | 🟡 | ❌ | 🟡 platform PITR, not via MCP | 🟡 platform logs | ✅ | ❌ OAuth | MCP + agent-skills | Apache-2.0 server, platform pricing |
| Neon MCP | ❌ (cloud) | 🟡 | 🟡 `readonly=true` hides write tools | 🟡 | ❌ | ✅ branches, snapshot restore | ❌ | ✅ | ❌ | MCP; migrations and tuning on a temp branch | MIT; "not for production" ([repo](https://github.com/neondatabase/mcp-server-neon)) |
| Prisma MCP | ❌ | ❌ | 🟡 ORM blocks `migrate reset` / `--accept-data-loss` when it detects an agent | 🟡 | ❌ | 🟡 Prisma Postgres only | ❌ | ✅ | 🟡 local server works with no account | MCP | Apache-2.0, platform |
| PlanetScale MCP | ❌ | 🟡 | ✅ short-lived `pg_read_all_data` role on a replica | 🟡 | ❌ | ❌ | ❌ | ✅ | ❌ | MCP (insights-only variant) | platform |
| Xata Agent (AI SRE) | ❌ | 🟡 | 🟡 playbooks | 🟡 | 🟡 playbooks | ❌ | 🟡 via CloudWatch metrics | ❌ own Postgres + CloudWatch | 🟡 self-host | MCP tools | Apache-2.0; **archived 2026-06-15** ([repo](https://github.com/xataio/agent)) |
| pganalyze (+ MCP preview) | n/a | ✅ | ✅ no direct DB access at all | ✅ | 🟡 history and workbooks | ❌ | 🟡 stored history | ❌ collector | ❌ SaaS | MCP (preview 2026.05) | paid ([docs](https://pganalyze.com/docs/mcp)) |
| Aiven AI DB Optimizer (ex-EverSQL) | n/a | n/a | n/a (advice only) | ❌ | ❌ | ❌ | ❌ | 🟡 | ❌ | ❌ ? | free tier ([Aiven](https://aiven.io/press/aiven-releases-ai-database-optimizer)) |
| DBtune | n/a | 🟡 | n/a (tunes config) | ❌ | ❌ | ❌ | ❌ | ❌ agent on the host | ❌ | ❌ ? | SaaS, pricing ? |
| pgAdmin 4 (9.13+) AI | ✅ server tunnel | ✅ GUI | ✅ READ ONLY + keyword allowlist (added after a txn-escape fix, [commit](https://github.com/pgadmin-org/pgadmin4/commit/bf4792444446f0e7ab721d23cbd6bfe6afaa7a8b)) | ❌ (GUI) | ❌ | 🟡 GUI wraps pg_dump | ❌ | ✅ | ✅ BYO key | ❌ | PostgreSQL lic. |
| DataGrip 2026.2 | ✅ | ✅ | 🟡 read-only connection flag ? for agent calls | 🟡 | 🟡 IDE context | ❌ | ❌ | ✅ | 🟡 JetBrains account | ✅ MCP tools + 3 agent skills ([blog](https://blog.jetbrains.com/datagrip/2026/07/16/datagrip-2026-2-ai-agent-skills-mcp-tools-and-cli-commands-for-data-source-management-bundled-jdbc-drivers-and-improved-session-control/)) | paid |
| DBeaver (PRO 26 / community MCP) | ✅ reuses saved tunnels | ✅ | 🟡 per-connection read-only ? | 🟡 | ❌ | 🟡 GUI wraps pg_dump | ❌ | ✅ | ✅ community | ✅ PRO MCP + `dbvr` CLI ([26.0](https://dbeaver.com/2026/03/12/dbeaver-26-0/)) | Apache-2.0 / paid |
| TablePlus AI | ✅ | ✅ | 🟡 safe mode | ❌ | ❌ | 🟡 GUI wraps dump | ❌ | ✅ | ✅ | ❌ ? (MCP claim unverified) | paid |
| Chat2DB | ✅ | ✅ | ❓ not found | 🟡 | ❌ | ❌ | ❌ | ✅ | 🟡 | MCP | source-available since 5.3.0 ([note](https://botmonster.com/coding/chat2db-license-change-source-available/)) |
| Vanna 2.0 (text-to-SQL) | ❌ | ❌ | ❌ user scope | 🟡 | 🟡 trained examples | ❌ | ❌ | ✅ | ✅ | framework | MIT; **archived 2026-03-29** |
| Outerbase | — | — | — | — | — | — | — | — | — | — | cloud closed 2025-10-15; Studio is AGPL ([Cloudflare](https://blog.cloudflare.com/cloudflare-acquires-outerbase-database-dx/)) |
| Tiger pg-aiguide / Supabase agent-skills | n/a | n/a | n/a (guidance only, no DB access) | n/a | n/a | ❌ | ❌ | ✅ | ✅ | ✅ skills + docs MCP ([pg-aiguide](https://github.com/timescale/pg-aiguide)) | open source |
| pgai (Tiger) | n/a | n/a | n/a | n/a | n/a | ❌ | ❌ | ❌ extension | ✅ | ❌ | **archived 2026-02-26**; pgvectorscale is still maintained |
| pgBackRest / WAL-G / Barman | 🟡 repo over SSH (Barman, pgBackRest) | 🟡 config stanzas | n/a | 🟡 `info --output=json` | ❌ | ✅ physical + PITR | ❌ | ❌ agent on the host / repo server | ✅ | ❌ **no MCP or skill** | MIT / Apache / GPL; pgBackRest unmaintained 2026-04-27, then revived 2026-05-18 ([news](https://pgbackrest.org/news.html)) |
| SimpleBackups | n/a (SaaS pulls) | ✅ | ✅ MCP cannot delete backups; `dry_run`; `restore-prep` only | ✅ | ❌ | ✅ many DBs, their dashboard | ❌ | ✅ | ❌ account | ✅ **official MCP, in Claude's directory** ✔ ([docs](https://simplebackups.com/docs/mcp/overview)) | SaaS per plan |
| Managed PITR (RDS / Supabase / Neon) | n/a | n/a | n/a | via cloud APIs | ❌ | ✅ PITR | 🟡 console metrics | ✅ | ❌ | partly via vendor MCP | provider pricing |

**What the matrix says.**
- Running SQL over MCP is commodity: more than 10 free servers do it.
- In 2025–26, "read-only" broke in the reference server, DBHub, AWS Labs (twice) and pgAdmin.
- Three things have no answer in any other single local tool:
  - SSH that honours `~/.ssh/config`
  - per-database memory
  - diagnosis while Postgres is down
- Only pgbx combines backups and an agent layer without a SaaS account.

---

## 2. Where pgbx is ahead (with evidence)

1. **The read-only guard covers the attacks that broke others.** `cli/src/query.rs:129-131` denies `set_config`
   (the AWS CVE-2026-85787 class), `dblink*` and `lo_*` (the DBHub CVE-2026-61788 payloads), `setval`/`nextval`,
   advisory locks and `pg_terminate_backend`. Other layers handle the rest:
   - The single-statement check stops the `COMMIT; DROP` escape that broke the reference server.
   - `COPY` is not an allowed first keyword, so the `COPY TO PROGRAM` RCE (CVE-2026-87911) is refused.
   - The query then runs inside `BEGIN READ ONLY` with timeouts, and is rolled back.
   - Unit tests cover these refusals (`query.rs:378-379`).

   Only pgEdge and Crystal (a real parser) are comparable. pgbx is also honest that this "is a guard, not a security
   boundary" (ADR 0002 §3), which is the lesson every one of those CVEs taught.
2. **SSH the way engineers already do it.** pgbx runs the system `ssh` with BatchMode, so `~/.ssh/config`, the ssh
   agent, ProxyJump and Windows OpenSSH all work. pgbx never sees a key, and one tunnel per profile is reused for
   10 min.
   - DBHub is the only maintained MCP with jump hosts, and it takes key paths in TOML rather than reading your ssh
     config.
   - Every vendor MCP assumes a cloud endpoint.
3. **Persistent per-database memory: nobody else has it.** `memories.md` and `tables.md` per connection and database
   hold saved named queries ("orders today") and what tables mean. They are plain files you own. The agent writes them
   only when asked, and `memories export/import` moves them without overwriting local edits. Other MCP servers
   rediscover the schema every session; the closest alternatives are hand-written YAML/TOML tools (Google, DBHub).
4. **Diagnosis when Postgres is down.** `pgbx diagnose` reads `postmaster.pid`, `pg_ctl`, the server log, the
   journal, kernel OOM, disk, `pg_wal`, replication slots and permissions. It returns `probable_cause`, `evidence`,
   and tiered `steps` that it never runs, and it never offers to delete `pg_wal`, `base`, `global` or `pg_xact`. Every
   MCP server needs a live SQL connection. Only pganalyze history and the now-archived Xata Agent come close, and both
   need a collector or a cloud account.
5. **Backups and an agent, self-hosted, with no account.**
   - pgBackRest, WAL-G and Barman have no agent layer at all.
   - SimpleBackups' MCP needs a SaaS account and stops at "restore-prep".
   - pgbx's agent can back up, **restore into a new database**, run a **verify restore that proves the dump works**
     (no one else offers an agent-run restore test), and restore from S3 onto a bare server.
6. **One JSON contract for every command.** Each command returns one object with `ok`, `command` and `safety`, and
   a non-zero exit on failure. That holds across query, health, backup, restore and diagnose. MCP servers return
   ad-hoc JSON text per tool.
7. **Safety tiers apply to every action, not only to SQL.** Guarded commands are refused without `--yes`, and the
   skill is told never to add `--yes` itself. Prisma's "AI consent" environment variable is the only similar idea in
   the market, and it covers a few ORM commands.
8. **It works across ecosystems and lasts.** pgbx is MIT, local-first, and runs anywhere (including Windows), against
   any Postgres, VPS or managed. Many alternatives in this list stalled, were archived or changed license in 2026:
   Crystal, Xata Agent, Vanna, pgai, the reference server, Chat2DB. pgBackRest briefly went unmaintained.

---

## 3. Where pgbx lacks (blunt)

1. **No MCP server.** It is the default way clients connect: Claude Desktop, Cursor, the Claude directory and
   DataGrip all speak MCP. pgbx reaches only skill-capable agents (Claude Code and Codex). Platform engineers
   evaluating "MCP servers" will not find it at all.
2. **No schema introspection helpers.** pgbx has no `pgbx schema` / `tables` / `describe`. The agent must write
   `information_schema` SQL itself, and `tables.md` is filled only on request. Every competing MCP ships list_schemas
   and describe_table on day one.
3. **No EXPLAIN ANALYZE, no tuning.** `EXPLAIN` without ANALYZE is allowed, but pgbx has:
   - no index advice (Crystal uses hypopg, pganalyze has Index Advisor, Aiven has its optimizer)
   - no `pg_stat_statements` top queries
   - no bloat or vacuum health check at the database level beyond `doctor()`'s backup focus
4. **No write workflows.** pgbx does no migrations, DDL or data fixes. Neon (temporary-branch migrations), Prisma and
   Supabase all offer guided write paths. Before a risky change pgbx can take a backup, but it cannot apply the change.
5. **The guard is word-level, not a parser.** The ADR admits it refuses harmless queries (a column named `"update"`).
   It also creates no read-only role, so the only hard boundary is a role the user must set up themselves. pgbx
   offers no helper for that and no `doctor` check that the connection role is read-only.
6. **Client-only users get friction.**
   - `setup client` treats a missing extension as `ok:false`.
   - `doctor`, `logs` and `diagnose` over SSH need pgbx installed on the host.
   - The site leads with backups, so the "client only" path is invisible.
7. **No GUI for querying.** `pgbx ui` is a read-only audit view of backups. GUI users (pgAdmin, DataGrip, DBeaver,
   TablePlus) get no pgbx value.
8. **Postgres only.** DBHub, Google Toolbox, Chat2DB and DBeaver cover MySQL, SQLite and more.
9. **Backup depth (for the backup buyer).**
   - No PITR on main: pgbx uses `pg_dump`, so the default schedule can lose up to a day of data.
   - No incremental or physical backups.
   - No encryption at rest by pgbx (it relies on the bucket's).
   - No published speed numbers (`positioning.md` §1).
10. **No proof in public:** no demo recording, no external users, and no install counter (`positioning.md` §1).

---

## 4. Top 5 moves, ranked by impact ÷ effort

| # | move | for | why (one line) | effort |
|---|---|---|---|---|
| 1 | **`pgbx mcp`: a stdio MCP server that wraps the existing CLI** (query, profiles, status, doctor, diagnose, memories read; safety tier from each command; guarded tools need a human-confirmed flag) | client/agent **and** backup | It opens every MCP client and the "MCP server" evaluation lists, with nothing new under it: the JSON contract already exists. | M (≈1 wk) |
| 2 | **Schema helpers + memory bootstrap:** `pgbx schema [--table T] --json` (tables, columns, PKs/FKs, row estimates, sizes), plus `pgbx memories init` that drafts `tables.md` from it for the user to approve | client/agent | It closes the most visible gap against every MCP, and turns memory from "write it yourself" into a reason to choose pgbx. | S (2–3 d) |
| 3 | **Client-only mode as a first-class path:** `setup client --no-extension` gives `ok:true`; a site page "pgbx as your agent's Postgres client (no backups needed)"; `doctor` checks whether the connection role can write and prints the one `CREATE ROLE … pg_read_all_data` line | client/agent | It removes the `ok:false` friction and turns the "not a security boundary" caveat into a one-command hard guarantee, which is what the CVEs say users need. | S (2–3 d) |
| 4 | **Health and tuning reads:** `pgbx health --json` (top `pg_stat_statements`, unused/missing indexes, bloat estimate, long transactions, locks), all through `query`'s guard | client/agent + DBA-ish | It matches Crystal and pganalyze's top asks locally with no collector, and ties into `diagnose` for a complete "why is my DB slow / down" story. | M (≈1 wk) |
| 5 | **Proof for the backup buyer:** merge v0.6 (PITR, encryption), publish bench numbers, and record "agent backs up before a migration, verifies, and restores from S3 onto a new box" | backup | Trust scores sit below 0.6 (`positioning.md` §4), and no competitor can show an agent-run restore test. Proof moves trust; copy does not. | M–L (PITR is the long pole) |

Not recommended now:
- **A GUI.** pgAdmin, DataGrip and DBeaver own that space.
- **Multi-database support.** It dilutes the Postgres depth.
- **Write and migration workflows.** They are high risk, and Neon and Prisma already own branch-based writes.
  Revisit after #1 to #3, and keep it to "backup first, then hand the change to the human".

---

## 5. Positioning one-liner for the agent/client angle (validated with jevx)

**Method.** I ran `jevx ask --states <persona | one-liner>.jsonl` with three questions:
- **try:** "Would this person try this product after reading the one-liner?"
- **clear:** "Is it clear to this person what the product does and why they need it?"
- **trust:** "Would this person trust it with access to their production database?"

The numbers are P(yes) from hosted Jev, 2026-10-02, on two personas:
- **dev**: an app developer new to Postgres who uses Claude Code; their DB is on a VPS reachable only over SSH.
- **plat**: a platform engineer evaluating MCP servers, who cares about read-only enforcement, bastions, and not
  installing anything on DB hosts.

| id | one-liner (shortened) | dev try / clear / trust | plat try / clear / trust |
|---|---|---|---|
| P1 | Safe agent access to Postgres behind SSH… profiles, jump hosts, read-only JSON, per-DB notes. No server install, no account. | .83 / .82 / .40 | .79 / .88 / .56 |
| P2 | Your agent can ask your production Postgres anything, and change nothing… | .78 / .78 / .65 | .71 / .80 / .56 |
| P3 | The Postgres client for coding agents: bastions via your ssh, typed JSON in a read-only txn, schema notes… | .84 / .81 / .46 | .80 / .87 / .49 |
| P4 | A Postgres CLI and Claude Code skill: profiles, tunnels, SELECT-only JSON, memory, backups, safety tiers. *(feature list)* | .80 / .71 / .42 | .71 / .75 / .45 |
| P5 | Stop pasting psql output into Claude… even explains why Postgres is down. | .82 / .83 / .39 | .65 / .80 / .29 |
| P6 | Read-only Postgres for AI agents through the ssh you already trust: no MCP server to host, no port, no SaaS… | .77 / .75 / .52 | .77 / .82 / .60 |
| P7 | Read prod Postgres and change nothing, through the ssh you trust… no MCP server to host, no port opened, no account. | .80 / .83 / .71 | .78 / .86 / .66 |
| **P8** | **see below** | **.79 / .80 / .70** | **.80 / .87 / .72** |
| P9 | Read-only Postgres for coding agents… nothing installed on the DB host… backups to S3 when you want them. | .81 / .80 / .53 | .82 / .87 / .66 |

**Pick: P8.** It is the only variant at or above 0.70 trust for both personas, with no loss on try or clear:

> **Your agent can ask production Postgres anything and change nothing. pgbx goes through your own ssh and jump
> hosts, runs every query in a read-only transaction with timeouts, and keeps what each table means in plain files
> you own. MIT, no account.**

**What moved the scores.**
- Trust jumps from about 0.4 to about 0.7 when the line names the **mechanism** ("read-only transaction with
  timeouts", "your own ssh"). It also jumps with **ownership** ("plain files you own").
- Feature lists (P4) and jokes (P5) score worst on trust.
- "Backups when you want them" adds nothing for the platform persona, so keep backups for the second sentence or
  the backup page.

**Honesty caveat.** "Change nothing" is stronger than the guard, which is documented as not a security boundary.
Ship it only alongside move #3: a `doctor` check for a read-only role, plus a one-line role recipe. Then the claim
is literally true for users who follow setup.

Do not change the live site until the owner agrees.
