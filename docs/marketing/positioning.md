# pgbx positioning (draft, 2026-10-02)

Draft for the owner. Nothing here has been posted, sent or published. Every claim cites a source, or is marked
**[hypothesis]**. Sources used: `README.md`, `CHANGELOG.md`, `site/src/content/docs/` (on `main`), branch `cli-profiles`,
branch `load-aware-backups` (`docs/adr/0001-load-aware-backups.md`), `~/muthu/gitworkspace/pgbx-pitr` (commit 580acac),
`~/muthu/gitworkspace/pgbx-v6extras` (commit b013673), `ceo-harness/NOW.md` and `LEARNED.md`.

**Thesis.** The pgbx extension does the work inside Postgres. The `pgbx` CLI is the client. The agent skill drives the
CLI. So a person, or an agent, can run backups and restores without writing scripts.

---

## 1. Where we stand

### Proven: shipped in v0.5.0 on `main`
| claim | source |
|---|---|
| Every database (and `template1`) is backed up with `pg_dump`/`pg_restore`, streamed to and from S3. No `archive_mode`. | CHANGELOG 0.5.0 |
| A new database gets its first backup on the next worker tick ("within a minute"). | README; ADR 0001 context (`src/worker.rs:300-304`) |
| Defaults: daily at 02:00, keep 14 backups, max 90 days. Schedule and retention are set in SQL. | README |
| Restores only go into a NEW database. The live database is never overwritten. | README "For AI agents"; `concepts/safety-tiers.md` |
| Restore onto a new server from S3 without the extension (`pgbx db-restore --from-s3`). | CHANGELOG; README |
| Weekly automatic restore test (`verify`): restore the newest backup into a scratch DB, check it, drop it. | README |
| `doctor()`, `overview()`, a read-only audit UI, and viewer/admin roles. | README |
| PostgreSQL 13 to 18. Linux install script; Windows install for the CLI and skill only. | README |
| Agent skill for Claude Code / Codex, with risky changes gated in code. | README; `skills/pgbx-skill` |
| MIT license, public docs site. | repo; pgbx-pitr 9fda391 |

### Built but not merged (in-flight)
| item | status | source |
|---|---|---|
| CLI profiles (`pgbx profile add/list/use`, `--profile`); the skill picks the server with `profile list --json` | 4 commits ahead of main, e2e test added | branch `cli-profiles` |
| SSH tunnels reused for 10 min; `pgbx query` (SELECT-only JSON for agents) | **not found** on `cli-profiles` by `git grep`; treat as planned | branch `cli-profiles` |
| Load-aware backups: wait when the DB is busy, suggest a quiet window, run at low CPU/IO priority | **ADR only, status Proposed**; no code | ADR 0001 |
| Point-in-time restore with a native engine (no pgBackRest) | separate repo, one commit | pgbx-pitr 580acac |
| Client-side encryption, roles in every backup, alerts, Prometheus metrics, GFS retention | separate repo, one commit | pgbx-v6extras b013673 |

### Not proven yet (say so plainly)
- **No published speed or size numbers.** `site/.../testing.md` lists every test duration as "to be re-measured".
  The bench covers ~1.2 GB against a local S3 only.
- **No external users or production stories** that we know of. No stars, installs or downloads are measured here.
- **Restores are per database at the time of a dump.** There is no point-in-time restore on `main`, so up to a
  day of data can be lost on the default schedule. Do not say "PITR" in any message until v0.6 is merged.
- **Backups run at normal priority with no load check today** (ADR 0001). A big dump at a busy hour can slow the app.
  Until ADR 0001 ships, the honest line is "pick a quiet hour" (the default is 02:00).
- **No encryption at rest by pgbx** on `main`; it relies on the bucket's own encryption.
- **Trust is our weakest score** (section 4): every variant scored below 0.6 on trust.

---

## 2. Segments

The first action is always a low-effort, measurable step, not a purchase.

### S1. App and full-stack developers new to Postgres, on a VPS (top segment)
- **Pain, in their words [hypothesis, to check against real threads]:** "I set up Postgres on a Hetzner box and I
  honestly don't know if my backups work." "My cron pg_dump job failed silently for weeks."
- **Promise:** Install once, give it a bucket, and every database is backed up and test-restored each week.
- **Proof needed:** a 3-minute recording from a fresh VPS to the first restore; a restore drill with real numbers.
- **Where they are:** r/selfhosted, r/PostgreSQL, r/webdev, Hetzner/Coolify/Dokku communities, HN "Ask HN: how do
  you back up Postgres" threads.
- **First action:** run the install and `pgbx doctor`, and see the first backup in `pgbx status`.

### S2. Indie SaaS with one database per customer (top segment)
- **Pain:** "I need to restore one tenant without rolling back everyone." "New customer databases are never in the
  backup script."
- **Promise:** Each customer database is backed up within a minute of being created and can be restored on its own.
- **Proof needed:** a demo with 50 databases, `pgbx.overview()`, and one restore of a single tenant.
- **Where they are:** Indie Hackers, r/SaaS, X build-in-public, multi-tenancy threads on HN.
- **First action:** call `pgbx.configure(...)` from a migration.

### S3. Agencies running many client databases
- **Pain:** "Every client server has a different backup script and nobody checks them."
- **Promise:** One CLI with a profile per client server; one status view across all of them.
- **Proof needed:** CLI profiles merged; a docs page "one laptop, ten client servers".
- **Where they are:** agency founders on X and LinkedIn, Laravel and Rails communities.
- **First action:** `pgbx profile add` for two servers, then `pgbx status --profile`.
- **Gap:** depends on `cli-profiles` being merged.

### S4. Platform and DevOps teams
- **Pain:** "pgBackRest is powerful but the config is a project in itself." They also need PITR, encryption and metrics.
- **Promise (later):** Backups set in SQL, scraped by Prometheus, restore-tested every week.
- **Proof needed:** v0.6 (PITR, encryption, metrics), plus published bench numbers. **Not a v0.5 target.**
- **Where they are:** r/devops, r/kubernetes, the PostgreSQL Slack, Postgres conference talks.
- **First action:** star the repo or watch releases; try it on a staging server.

### S5. AI-agent builders who want an agent to operate the database (top segment, our wedge)
- **Pain:** "I won't give an agent shell access to prod Postgres. One wrong command and the data is gone."
- **Promise:** The agent drives the pgbx CLI through a skill; restores never touch the live database, and risky
  steps are blocked in code, not by the prompt.
- **Proof needed:** a recording of Claude Code doing "back up prod before this migration, then verify"; the
  skill's self-test (`skills/pgbx-skill/tests/all.sh`) passing in public CI.
- **Where they are:** Claude Code / Codex users on X, r/ClaudeAI, MCP and agent-tooling threads, HN.
- **First action:** `sh skills/pgbx-skill/install.sh` and ask the agent for `status`.

### S6. Windows and macOS developers who only use the CLI against remote servers
- **Pain:** "My database is on a Linux server; I just want to check and restore it from my laptop."
- **Promise:** The pgbx CLI on your laptop talks to your server's pgbx; restores from S3 need only `pg_restore`.
- **Proof needed:** the Windows installer tested on the `pgbx-win` runner; macOS binary; SSH tunnel feature.
- **Where they are:** same as S1 and S3.
- **First action:** install the CLI and run `pgbx backups --from-s3`.
- **Gap:** SSH tunnels are not in code yet (section 1).

**Order to work in:** S1 and S5 first (the product is ready for them today), S2 next (it is ready, it needs a demo),
then S3 and S6 after `cli-profiles` merges, then S4 after v0.6.

---

## 3. Competition: the products only

What users complain about is from common public discussion and the tools' own docs. It is **[hypothesis]** until we
link the actual threads, which is also the first launch task (section 6).

| product | what it does well | common complaints | pgbx is better at | pgbx is worse at |
|---|---|---|---|---|
| **pgBackRest** | physical backups, PITR, parallel, incremental, encryption, very mature | config files, stanzas, archive setup, learning curve for non-DBAs | zero config, per-database restore, SQL control, agent skill, auto-pickup of new DBs | no PITR (until v0.6), no incremental, slower on large clusters (pg_dump based), much less proven |
| **WAL-G** | WAL archiving + PITR to object storage, fast, multiple databases engines | env-var config, docs are thin, restore steps are manual | restore into a new DB with one command, verify drills built in, status/doctor | no PITR, no delta backups |
| **Barman** | central backup server for many Postgres servers, PITR, mature | needs its own server and SSH/streaming setup | no extra server; runs inside Postgres | no central multi-server catalog yet (profiles help) |
| **pg_dump + cron scripts** | simple, everyone understands it, free | fails silently, no retention, no restore testing, new DBs missed | same pg_dump underneath, plus schedule, retention, verify, new-DB pickup, alerts on failure | none on simplicity of concept; one more thing to install |
| **SimpleBackups and hosted backup SaaS** | nice dashboard, alerts, many DB types, no install | subscription per DB, credentials go to a third party, data path via their servers | free, MIT, your bucket, no account, credentials stay on your server | no hosted dashboard or email alerts on main |
| **Managed DB built-in backups (RDS, Supabase, Neon, DO)** | nothing to run, PITR included | locked to the provider, hard to get an off-site copy, per-DB restore can be awkward | works on any VPS, off-site copy in a bucket you choose | if you are on managed Postgres already, you mostly don't need pgbx |

**Honest summary.** pgbx does not win on raw power against pgBackRest, WAL-G or Barman. It wins on **time to the first
tested restore** for people who are not DBAs, on **per-database** handling, and on being **operable by an agent**. Say
"for developers, not DBAs" and do not pick fights on throughput.

---

## 4. Message house

- **Roof:** Your Postgres backed up, and the restore proven, without becoming a DBA.
- **Pillars:**
  1. *Nothing to remember.* New databases are picked up within a minute; defaults are sane (README).
  2. *Proven restores.* A weekly verify restores the newest backup into a scratch database (README).
  3. *Safe by design.* Restores only go to a new database; risky steps are gated in code (safety-tiers doc).
  4. *Agent-operable.* Extension, CLI and skill: an agent can run it (README "For AI agents").
  5. *Yours.* MIT, your S3 bucket, no account (README, site hero).
- **Foundation (proof):** open code, public docs, e2e tests in repo. **Missing:** published numbers and a demo recording.

### Hero variants, scored with jevx

Method: `jevx ask --states heroes.jsonl --noul install=... --noul clear=... --noul trust=...` (2026-10-02). Each input
is a persona line plus the hero. Questions: *install* "Would this person install it after reading this?", *clear*
"Is it clear to this person what the product does and why they need it?", *trust* "Would this person trust it with
their production data?". Numbers are P(yes). The earlier site score (0.72 / 0.69 / 0.49) used a different persona
prompt, so A1 below is not directly comparable.

| id | segment | hero | install | clear | trust |
|---|---|---|---|---|---|
| A1 | S1 | Never lose your app's database. Automatic Postgres backups for developers, not DBAs: install once, give it an S3 bucket, every database is saved nightly. Restore with one command, even onto a new server. Free, open source, no account. *(current site)* | 0.78 | 0.87 | 0.40 |
| A2 | S1 | Your Postgres has no backup until you test a restore. pgbx backs up every database to your own S3 bucket and restores last night's copy into a new database every week to prove it works. One install, MIT licensed. | 0.74 | 0.85 | **0.49** |
| A3 | S1 | Backups without cron scripts. One install inside Postgres, three commands, and every database on your VPS goes to your S3 bucket on a schedule. pgbx doctor tells you in plain words when something is wrong. | 0.76 | 0.83 | 0.36 |
| B1 | S2 | One database per customer, one backup per database. Create a customer database and pgbx backs it up within a minute; restore just that customer into a new copy without touching anyone else. | 0.78 | 0.88 | 0.47 |
| B2 | S2 | Restore one customer, not the whole server. Separate S3 backup for every database, own schedule and retention, set by one SQL call from your migrations. | 0.78 | 0.87 | 0.50 |
| B3 | S2 | Every customer database backed up, checked weekly, listed in one view. pgbx.overview() shows each database's last backup; failed ones explain themselves. Free and open source, your bucket. | **0.82** | **0.88** | **0.58** |
| C1 | S5 | Let your agent run Postgres backups safely. An agent skill drives the pgbx CLI: status, backup before deploy, restore into a new database only. Destructive actions are gated in code, not in the prompt. | 0.74 | 0.83 | 0.55 |
| C2 | S5 | A database admin your agent can operate. The extension does the work inside Postgres, the CLI gives JSON, the agent skill drives it. Restores never overwrite a live database. | 0.65 | 0.74 | 0.36 |
| C3 | S5 | Ask your coding agent "back up prod before this migration" and it does, with proof. Postgres backups to your S3, a CLI with --json, and a Claude Code / Codex skill with safety gates. | **0.77** | **0.87** | **0.56** |

**What the scores say.** Clarity is solved (0.83 to 0.88 except C2). Trust is the gap: 0.36 to 0.58. The variants
that score highest on trust name a *check* ("checked weekly", "with proof", "gated in code"). Architecture language
(C2) scores worst. No copy can close the trust gap on its own; it needs public proof (section 6). Picks: **A2** for S1
(replace "nightly" with "tested"), **B3** for S2, **C3** for S5. Do not change the live site until the owner agrees.

---

## 5. Offer and packaging [all hypothesis]

- **Free OSS core, forever, MIT:** everything in v0.5 and everything planned for v0.6 (PITR, encryption, metrics,
  GFS retention, profiles, load-aware backups). Rule: **nothing that keeps data safe is ever paid.**
- **Possible paid, later, only if users ask for it:**
  - *Hosted dashboard:* many servers in one view, history, team logins. The OSS `pgbx ui` stays local and read-only.
  - *Alerts delivered:* email/Slack/WhatsApp when a backup or verify fails, without running your own alerting.
    (v6extras notifications stay free; the paid part is delivery and escalation.)
  - *Managed restore drills:* a monthly drill on our runner with a signed report (time to restore, row counts) for
    audits and customers.
  - *Support/setup for agencies (S3):* a fixed-price install across client servers. This fits the current priority
    of recurring income (NOW.md) better than a SaaS, and needs no new code.
- **Test before building:** count requests in issues and replies for each item over 30 days; build none of it before
  at least 3 independent asks.

---

## 6. 30-day launch plan (proof first)

Built on LEARNED.md: interrupt channels measured about 0 (lesson 2); the only replies came from engaging with what
people themselves wrote, carrying our own measurement (lesson 3 and the "measurement WE ran" note); agent velocity on public surfaces burns
accounts (lesson 4; Reddit `u/deemwar` shadowbanned, GitHub `deemwario` flagged); original posts measured zero reach
(2026-09-08); hold a strategy 30 days unless data changes it (2026-09-12). **Every public action below is drafted by the
agent and posted by the owner, by hand, at human pace.**

| week | build the proof | engage (owner posts) |
|---|---|---|
| 1 (Oct 3 to 9) | Re-measure `testing.md` and publish real numbers. Record a 3-minute demo: fresh VPS, install, first backup, `db-restore --from-s3` onto a second server (use `demo-recorder`). Merge `cli-profiles`. | Collect 30 existing threads where people asked how to back up Postgres or lost data; link them in `docs/marketing/threads.md`. This also replaces the [hypothesis] labels in sections 2 and 3 with real quotes. |
| 2 (Oct 10 to 16) | Blog: "Restore drill: we deleted a server and timed getting every database back", with real sizes and times from week 1. Agent demo: Claude Code backs up before a migration and verifies (C3). | Answer at most 2 threads a day, only where our drill numbers answer the question asked. No links unless asked. |
| 3 (Oct 17 to 23) | Fix whatever the first users hit; write it in CHANGELOG. Start ADR 0001 phase 1 if busy-hour slowdown comes up. | Same reply cadence. One post on our own company page only (allowed since 09-29). |
| 4 (Oct 24 to 31) | Show HN **only if** the demo, the drill blog and published numbers all exist. Title idea: "Show HN: pgbx, Postgres backups that test their own restores". | Owner stays in the HN thread for the first 4 hours; answers with numbers, not adjectives. |

**Metrics (measure weekly; set targets after week 1 gives a baseline, do not invent them now):**
- Proof: demo recorded (y/n), drill blog live (y/n), testing.md has numbers (y/n).
- Engagement: replies drafted, replies posted, **human replies received** (a "no thanks" is not a reply, per LEARNED
  2026-09-12), conversations (2+ exchanges).
- Adoption: GitHub stars, install-script fetches (needs a counter; none today), issues opened by people we don't know.
- Health: zero account flags or shadowbans (check Reddit anonymous view weekly).
- Copy: re-run the jevx scores after each proof ships; the target is trust above 0.6 with no copy change, which
  would show that proof, not words, moves it **[hypothesis]**.

**Not in this plan:** cold email, ads, LinkedIn outreach, posting in other people's repos. All measured about 0 or
burned accounts (LEARNED lessons 2 and 4).

---

## Appendix: proposed homepage diagram

`docs/marketing/diagrams/HowItWorks.astro` is a **proposal** for the homepage and the how-it-works page (it is not in
`site/` here, to avoid colliding with `cli-profiles`). It shows: you or an agent (pgbx-skill) → pgbx CLI (profiles,
`--json`, `query`, `tunnel`, `--yes` gates) → SQL directly or over an SSH tunnel with jump hosts → the pgbx extension
inside Postgres (install once) → your S3 bucket, plus `db-restore --from-s3` when the server is gone. Command names
follow branch `cli-profiles` as reported by its agent (not merged yet).
