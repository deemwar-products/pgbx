#!/usr/bin/env bash
#
# Router stress test: phrase -> expected route, using the documented
# longest-match tie-break. Also checks each route has id/mode/when/goal/step.
# Exit code = count of failures. bash 3.2 compatible.
set -u -o pipefail

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
SKILL_DIR=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)

SKILL_DIR="$SKILL_DIR" python3 - <<'PY'
import os, sys, collections
import xml.etree.ElementTree as ET
refs = os.path.join(os.environ["SKILL_DIR"], "references")
fails = 0
def bad(m):
    global fails; fails += 1; print("  FAIL " + m)
root = ET.parse(os.path.join(refs, "router.xml")).getroot()
routes = root.findall(".//route")
def phrases(rt):
    out = []
    for w in rt.findall("when"):
        out += [p.strip().lower() for p in (w.text or "").split(",") if p.strip()]
    return out
for rt in routes:
    rid = rt.get("id")
    if not rid or not rt.get("mode") or not phrases(rt) or rt.find("goal") is None or not rt.findall(".//step"):
        bad(f"route {rid} incomplete")
print(f"  ok  {len(routes)} routes structurally complete")
owner = collections.defaultdict(set)
for rt in routes:
    for p in phrases(rt): owner[p].add(rt.get("id"))
for p, ids in owner.items():
    if len(ids) > 1: bad(f"phrase '{p}' claimed by {sorted(ids)}")
expected = [rt.get("id") for rt in routes]
for need in ["status","backup","backup-before-deploy","restore-db","disaster-restore","verify","policy","data-scope","download-link","troubleshoot","diagnose","audit-ui"]:
    if need not in expected: bad(f"missing route {need}")
def route(text):
    t = text.lower(); best = None
    for p, ids in owner.items():
        if p in t and (best is None or len(p) > len(best[0])): best = (p, ids)
    return next(iter(best[1])) if best else None
corpus = [
    ("is postgres backed up?", "status"),
    ("list backups for myapp", "status"),
    ("take a backup of myapp", "backup"),
    ("please backup before deploy", "backup-before-deploy"),
    ("restore the db from yesterday", "restore-db"),
    ("postgres is down, help", "diagnose"),
    ("we lost the server, restore on a new server", "disaster-restore"),
    ("the disk is full on db1", "diagnose"),
    ("pg_wal is huge", "diagnose"),
    ("postgres won't start after reboot", "diagnose"),
    ("pgbx diagnose", "diagnose"),
    ("restore from s3 onto db2", "disaster-restore"),
    ("verify the backup", "verify"),
    ("change backup schedule to hourly", "policy"),
    ("pause backups while I migrate", "policy"),
    ("exclude table from backup: sessions", "data-scope"),
    ("give me a download link", "download-link"),
    ("backups are failing since monday", "troubleshoot"),
    ("pgbx doctor", "troubleshoot"),
    ("who paused backups last week?", "audit-ui"),
    ("open the audit ui", "audit-ui"),
    ("pgbx ui", "audit-ui"),
]
for text, want in corpus:
    got = route(text)
    if got != want: bad(f"'{text}' -> {got}, expected {want}")
    else: print(f"  ok  '{text}' -> {got}")
print(f"  fail={fails}")
sys.exit(min(fails, 255))
PY
