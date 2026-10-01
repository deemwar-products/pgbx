#!/usr/bin/env bash
#
# Recipe format + cross-reference validator for pgbx-skill.
#   - SKILL.md frontmatter has name: pgbx-skill and a description with "Trigger:"
#   - every family file named in the "Families at a glance" table exists
#   - every `### <ID>:` recipe block has When to use / Command (or Call sequence) /
#     Expected response / Common errors / User-visible formatting
#   - no duplicate recipe IDs across all family files
#   - every recipe ID cited anywhere in references/ exists
#   - every router <step ref> points at an existing file
# Exit code = count of failures. bash 3.2 compatible.

set -u -o pipefail

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
SKILL_DIR=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)

SKILL_DIR="$SKILL_DIR" python3 - <<'PY'
import os, re, sys
import xml.etree.ElementTree as ET
skill = os.environ["SKILL_DIR"]; refs = os.path.join(skill, "references")
fails = 0
def ok(m): print("  \033[32m✓\033[0m " + m)
def bad(m):
    global fails; fails += 1; print("  \033[31m✗\033[0m " + m)

# frontmatter
s = open(os.path.join(skill, "SKILL.md")).read()
m = re.match(r"---\n(.*?)\n---\n", s, re.S)
if not m: bad("SKILL.md: no YAML frontmatter")
else:
    fm = m.group(1)
    ok("SKILL.md: frontmatter present") if re.search(r"^name: pgbx-skill$", fm, re.M) else bad("SKILL.md: name must be pgbx-skill")
    ok("SKILL.md: description has Trigger:") if re.search(r"^description: .*Trigger:", fm, re.M) else bad("SKILL.md: description lacks 'Trigger:'")
for marker in ["<!-- version:", "## Core Rules", "## Session Context", "## Process", "## Self-check", "## Families at a glance"]:
    ok(f"SKILL.md: has {marker}") if marker in s else bad(f"SKILL.md: missing {marker}")

# families
table = s.split("## Families at a glance", 1)[-1]
fams = re.findall(r"`references/([a-z-]+\.md)`", table)
if not fams: bad("Families table lists no files")
for f in fams:
    ok(f"family file {f} exists") if os.path.isfile(os.path.join(refs, f)) else bad(f"family file {f} missing")

# recipes
labels = {
  "When to use": r"\*\*When to use:\*\*",
  "Command": r"\*\*(Command|Call sequence):\*\*",
  "Expected response": r"\*\*Expected response[^*]*:\*\*",
  "Common errors": r"\*\*Common errors:\*\*",
  "User-visible formatting": r"\*\*User-visible formatting[^*]*:\*\*",
}
all_ids = {}
for f in fams:
    p = os.path.join(refs, f)
    if not os.path.isfile(p): continue
    blocks = re.split(r"^### ", open(p).read(), flags=re.M)[1:]
    if not blocks: bad(f"{f}: no recipe blocks")
    for b in blocks:
        rid = b.split(":", 1)[0].strip()
        if rid in all_ids: bad(f"duplicate recipe ID {rid} ({all_ids[rid]} and {f})")
        all_ids[rid] = f
        missing = [k for k, rx in labels.items() if not re.search(rx, b)]
        bad(f"{f} {rid}: missing {', '.join(missing)}") if missing else None
        if "```" not in b and "Call sequence" not in b: bad(f"{f} {rid}: no command block")
    ok(f"{f}: {len(blocks)} recipes checked")

# cited IDs resolve
cited = set()
for root, _, files in os.walk(refs):
    for fn in files:
        if fn.endswith(".md"):
            cited |= set(re.findall(r"\b(?:STAT|BKP|RST|VFY|POL|ACC|DIAG)-R-\d+\b", open(os.path.join(root, fn)).read()))
for c in sorted(cited):
    None if c in all_ids else bad(f"cited recipe {c} does not exist")
ok(f"{len(cited)} cited recipe IDs checked")

# router refs
try:
    r = ET.parse(os.path.join(refs, "router.xml")).getroot()
    for st in r.iter("step"):
        ref = st.get("ref")
        None if os.path.exists(os.path.join(refs, ref)) else bad(f"router step {ref} missing")
    ok("router step refs checked")
except ET.ParseError as e:
    bad(f"router.xml parse error: {e}")
print(f"  fail={fails}")
sys.exit(min(fails, 255))
PY
