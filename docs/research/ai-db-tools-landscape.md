# Postgres backup tools and AI/agent database tools: the landscape, and where pgbx stands (research, 2026-10-02)

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

## 0. What pgbx is (and is not)

**pgbx is a Postgres backup and restore product.** The agent is a secondary way to drive it. The order matters:

1. **Primary: backup and restore (the extension, inside Postgres 13–18).**
   - per-database `pg_dump` to your S3 bucket, picked up within a minute of `CREATE DATABASE`
   - schedules and retention set in SQL
   - a weekly **verify** restore that proves the newest dump works
   - restores **only into a NEW database**, never over the live one
   - `db-restore --from-s3` onto a bare server with no extension
   - `doctor()`, `overview()`, and a read-only audit UI
   - a server-wide job queue with coalescing and cancel, and resource caps (ADR 0001)
2. **The interface: one CLI (`pgbx`), for people and agents alike** (Linux, macOS arm64, Windows):
   - named profiles: locations only, never secrets
   - SSH through the system `ssh`, so `~/.ssh/config`, the ssh agent and jump hosts all work; one tunnel per
     profile is reused and closes after 10 min unused
   - `--json` on every command, which prints one object with `ok`, `command` and `safety`
   - `diagnose` and `doctor` when Postgres is down
   - `pgbx query`: SELECT-only typed JSON, with a four-layer guard ending in `BEGIN READ ONLY` + timeouts + ROLLBACK
   - per-database memory (`memories export|import|path`)
3. **Secondary: the agent skill (Claude Code and Codex)** drives **only the CLI**, never ssh or psql, under the same
   safety tiers. `--yes` counts as the human's signature, and the agent never adds it.

**Deliberate non-goals. These are choices, not gaps:**
- **No MCP server.** The interface is the CLI plus a skill. Every guard lives in one binary that a human can also
  run, read and audit. An MCP server would add a second surface to secure. Most of the 2025–26 read-only CVEs below
  are MCP servers.
- **No writes to user data.** pgbx never runs DDL, migrations or data fixes, and `query` is read-only. Restores
  create a new database; swapping or dropping the live database is left to a human. For a backup tool, caution is
  the product.
- **No GUI query client.**

**Day-to-day client use is real, but secondary.** Some people use only the CLI: `profile`, `query`, `tunnel` and
`memories` work against plain Postgres with no extension (`tests/ssh_e2e.sh`). They usually arrive through backups,
and the client path should stay smooth for them.

---

## 1. Capability matrix

Key: ✅ yes · 🟡 partial · ❌ no · ? = UNVERIFIED.

| tool | SSH / jump host | profiles | read-only guard / safety tiers | JSON for agents | per-DB memory / notes | backups / restore | diagnose when PG is down | no server components | local-first, no account | agent skill / MCP | license / price |
|---|---|---|---|---|---|---|---|---|---|---|---|
| **pgbx** | ✅ system ssh, `~/.ssh/config`, jump host, tunnel reused for 10 min | ✅ | ✅ 4 layers (1 stmt, allowlist, fn deny-list, READ ONLY txn) + 4 tiers for every command | ✅ one object per command (`ok/command/safety`) | ✅ Markdown files per DB, export/import | ✅ extension: S3, retention, verify, restore to a new DB, restore from S3 onto a bare server | ✅ `diagnose` (logs, OOM, disk, WAL, slots) | 🟡 query: none; backups need the extension; diagnose needs the CLI on the host | ✅ | ✅ agent skill over the CLI (no MCP, by design) | MIT, free |
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
- Most rows are AI query tools with no backups. pgbx competes with them only on its **secondary** surface (the
  CLI and skill). Its primary competitors are the backup rows at the bottom.

---

## 2. Where pgbx is ahead (with evidence)

### Primary: as a backup and restore product

1. **Restore tests built in.** A weekly verify restores the newest dump into a scratch database, checks it, then
   drops it.
   - pgBackRest, WAL-G and Barman leave restore testing to your own scripts.
   - SimpleBackups' MCP stops at "restore-prep".
   - Managed PITR never proves your off-site copy.
2. **Restores can't overwrite live data.** Every restore goes into a NEW database, and `db-restore` refuses an
   existing database or the source. The rule is enforced in code (safety-tiers doc) and holds whether a human or an
   agent issues the command.
3. **Zero config, and per database.** New databases are backed up within a minute. Schedule and retention are one
   SQL call, so they can live in a migration (`pgbx.configure`). pgBackRest needs stanzas plus archive setup, and
   Barman needs its own server. Neither restores a single tenant's database on its own.
4. **Disaster path with no extension.** `db-restore --from-s3` restores onto any Postgres, including a brand-new
   server, using only `pg_restore`.
5. **Diagnosis when Postgres is down.** `pgbx diagnose` reads `postmaster.pid`, `pg_ctl`, the server log, the
   journal, kernel OOM, disk, `pg_wal`, replication slots and permissions. It returns `probable_cause`, `evidence`,
   and tiered `steps` that it never runs, and it never offers to delete `pg_wal`, `base`, `global` or `pg_xact`.
   - No backup tool does this.
   - Every AI database tool needs a live SQL connection.
   - Only pganalyze history and the archived Xata Agent come close, and both need a collector or a cloud account.
6. **Yours, and stable.** MIT, your bucket, no account, and credentials stay on your server. That matters now,
   because pgBackRest briefly went unmaintained (2026-04-27 to 2026-05-18), and SimpleBackups and managed backups
   are tied to an account.

### Secondary: the CLI and skill, compared with AI database tools

7. **A cautious guard that blocks the attacks that broke other tools' read-only modes.** `cli/src/query.rs:129-131`
   denies `set_config` (the AWS CVE-2026-85787 class), `dblink*` and `lo_*` (the DBHub CVE-2026-61788 payloads), and
   `setval`/`nextval`. Other layers handle the rest:
   - The single-statement check stops the `COMMIT; DROP` escape that broke the reference server.
   - `COPY` is not an allowed first keyword, so the `COPY TO PROGRAM` RCE (CVE-2026-87911) is refused.
   - Unit tests cover these refusals (`query.rs:378-379`).

   pgbx also says plainly that this is a guard, not a security boundary (ADR 0002 §3).
8. **SSH the way engineers already use it.** The system `ssh` means `~/.ssh/config`, the ssh agent, ProxyJump and
   Windows OpenSSH all work, and pgbx never sees a key. DBHub is the only maintained tool with jump hosts, and it
   takes key paths in TOML.
9. **Per-database memory: nobody else has it.** `memories.md` and `tables.md` are plain files you own. The agent
   writes them only when asked, and they move between machines with `memories export/import`.
10. **One JSON contract and safety tiers for every action:** query, backup, restore and diagnose alike. The only
    similar idea in the market is Prisma's "AI consent" environment variable, and it covers a few ORM commands.

---

## 3. Where pgbx lacks (blunt)

No MCP and no writes are left out on purpose (see §0). The real gaps follow.

### Backup and restore (primary)
1. **No PITR on main.** Backups are `pg_dump` snapshots, so the default daily schedule can lose up to a day of
   data. This is the first objection from anyone who knows pgBackRest, WAL-G or managed PITR. The v0.6 work exists
   in a separate repo but is not merged.
2. **No encryption at rest by pgbx.** It relies on the bucket's own encryption. The client-side encryption work is
   also not merged.
3. **No incremental or physical backups.** On large clusters, `pg_dump` is slower than pgBackRest or WAL-G.
4. **No published proof:** no bench numbers (`testing.md` says "to be re-measured"), no recorded restore drill, no
   external users, and no install counter (`positioning.md` §1).
5. **No single status view across servers.** Profiles exist, but `status` and `overview` look at one server at a
   time. An agency with ten client servers checks them one by one.
6. **Restore proof isn't shareable.** Verify runs, but there is no report to show an auditor or a customer
   (duration, row counts, checksum).

### CLI and agent (secondary)
7. **Friction for client-only users.** `setup client` reports `ok:false` when the extension is missing. `doctor`,
   `logs` and `diagnose` over SSH need pgbx on the host.
8. **The guard is keyword-based, not a parser.** It refuses harmless queries (a column named `"update"`). There is
   also no check that the connection role is read-only, which is the only hard boundary.
9. **No schema helper.** The agent writes `information_schema` SQL itself, and `tables.md` is filled only on
   request.
10. **Postgres only, and no GUI.** That is fine for the focus, but DBeaver, DataGrip and DBHub users won't see pgbx.

---

## 4. Top 5 moves, ranked by impact ÷ effort

| # | move | for | why (one line) | effort |
|---|---|---|---|---|
| 1 | **Publish proof:** re-measure and publish bench numbers; record a drill: fresh VPS, first backup, verify, `db-restore --from-s3` onto a second box | backup | Trust scores sit below 0.6, and a restore drill with real numbers is what no competitor shows. | S (2–3 d) |
| 2 | **Merge v0.6: PITR + client-side encryption** | backup | It removes the first objection DBAs and platform teams raise, and gives the backup buyer parity with pgBackRest and WAL-G. | L (PITR is the long pole) |
| 3 | **`pgbx status --all` across profiles, plus a verify report** (`pgbx verify --report`: duration, row counts per table, size, dump key) | backup (agencies, SaaS) | One command answers "are all my servers backed up and restorable?", for a person or an agent. | S–M (3–5 d) |
| 4 | **A smooth client-only path:** `setup client` works without the extension (`ok:true`, backups shown as "not installed" with the one-line install); `doctor` warns when the connection role can write and prints the `pg_read_all_data` role line | client / agent | It lets day-to-day users start without backups, and turns the guard into a hard guarantee with one command, still read-only. | S (2–3 d) |
| 5 | **`pgbx schema --json` + `memories init`** (tables, columns, keys, sizes through the read-only path; it drafts `tables.md` for the user to approve) | client / agent | Agents stop guessing table names, and memory becomes useful from the first session. Reads only. | S (2–3 d) |

Explicitly **not** recommended:
- an MCP server
- write or migration workflows
- a GUI query client
- support for databases other than Postgres

Each one widens the attack surface or dilutes the focus of a backup tool whose promise is caution.

---

## 5. Positioning one-liner (backup first, agent second; validated with jevx)

**Method.** I ran `jevx ask --states <persona | one-liner>.jsonl` with three questions:
- **try:** "Would this person try this product after reading the one-liner?"
- **clear:** "Is it clear to this person what the product does and why they need it?"
- **trust:** "Would this person trust it with their production database?"

The numbers are P(yes) from hosted Jev, 2026-10-02. Personas:
- **vps**: the primary backup buyer, an app developer on a VPS with no tested backups who is not a DBA.
- **dev**: an app developer new to Postgres who uses Claude Code; their DB is reachable only over SSH.
- **plat**: a platform engineer evaluating tools that let agents touch company Postgres.

| id | one-liner (shortened) | vps t / c / trust | dev t / c / trust | plat t / c / trust |
|---|---|---|---|---|
| B1 | Postgres backups that prove they restore… S3, weekly test restore, only into a new DB… CLI over your ssh, or let your agent run the same CLI. MIT, no account. | .82 / .87 / .57 | .82 / .82 / .62 | .67 / .66 / .50 |
| B2 | Every DB backed up to your S3 and test-restored weekly. One CLI over ssh and jump hosts; your agent uses the same CLI… | .79 / .87 / .44 | .79 / .79 / .48 | .72 / .74 / .46 |
| **B3** | **see below** | **.83 / .88 / .62** | **.84 / .84 / .67** | **.76 / .82 / .61** |
| B4 | Backed up and restore-tested, without becoming a DBA. One CLI… agent drives it through a skill… | .80 / .88 / .39 | .79 / .83 / .44 | .68 / .73 / .42 |
| B5 | Backups to your S3, restore-tested, restored only into a new DB. The CLI and skill never write to your data… | .77 / .83 / .55 | .80 / .77 / .60 | .72 / .68 / .55 |

**Pick: B3.** It scores highest on every question for every persona:

> **Backups first, agents welcome. pgbx backs up each Postgres database to your S3, proves the restore every week,
> and gives you and your coding agent one cautious CLI: read-only queries, restores only into a new database, and
> risky steps wait for your --yes.**

**What moved the scores.**
- Trust rises when the line names **checks the human keeps** ("proves the restore", "wait for your --yes",
  "only into a new database").
- Trust falls when the line describes the architecture ("drives the same CLI through a skill", B4).
- The earlier agent-first variants scored higher on trust for the platform persona (0.72), but they misdescribe the
  product. B3 keeps backups as the headline and the caution as the reason to let an agent near it.

Note: these scores are from a model, not from users. Trust stays around 0.6 to 0.67 until the move-1 proof exists,
and copy alone won't close that gap. Do not change the live site until the owner agrees.
