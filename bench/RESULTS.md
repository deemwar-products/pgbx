# Backup overhead on the application (ADR 0001 §3, M8)

`bench/load_overhead.sh`: pgbench against a database while pgbx backs **the same database** up back to back
(pg_dump → zstd:3 → local S3), three ways. Budget (ADR 0001): with caps, p95 latency **+≤ 15 %**, TPS **−≤ 10 %**
against no backup.

## 0.6.0 — 2026-10-02, local (one run each)

pgbench scale 50 (756 MB), 16 clients, 4 threads, 120 s per run (the ADR's 300 s shortened for a local run).
Docker on Darwin arm64 (OrbStack VM, 18 cores visible), PostgreSQL 16.15, local RustFS S3 on the same network,
`pgbx.dump_compression = auto` (zstd:3). Command: `SCALE=50 CLIENTS=16 SECS=120 bench/load_overhead.sh`.

| run | backups during run | TPS | vs none | avg latency ms | p95 ms | vs none |
|---|---|---|---|---|---|---|
| none | 0 | 365 | | 43.78 | 147.98 | |
| uncapped (nice 0, ionice none) | 14 | 338 | −7.3 % | 45.45 | 160.38 | +8.4 % |
| capped (defaults: nice 10, best-effort-7) | 14 | 336 | −8.1 % | 46.15 | 164.17 | +10.9 % |

**Within budget** (−8.1 % TPS, +10.9 % p95 with 14 dumps of the same database in 120 s, i.e. a dump running the
whole time). Reading it honestly:

- Capped is **not** better than uncapped here. With 18 idle cores there is no CPU contention for `nice` to resolve,
  and the cost a dump puts on the app is on the Postgres side (the backend serving COPY, shared buffers, the disk),
  which pgbx does not renice (ADR 0001 §3, "the honest limit"). The ~1 % difference is within run-to-run noise of a
  single 120 s run.
- Caps are expected to matter on a CPU-saturated box (few cores, compression competing with the app). Run the
  script there (`SECS=300`, the self-hosted runners) before claiming more; record each run as a new section here.
- The real lever stays the schedule: `pgbx schedule suggest` (quiet window) and, as a safety net, the load gate.
