#!/usr/bin/env bash
#
# Idempotency test for install.sh / uninstall.sh.
#
# Uses a throwaway CLAUDE_SKILLS_DIR so we don't touch the real ~/.claude/skills.
# Asserts:
#   - first install creates a symlink pointing at the skill dir
#   - second install is a no-op (idempotent, no error)
#   - uninstall removes the symlink
#   - second uninstall is a no-op (idempotent)

set -u -o pipefail

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
SKILL_DIR=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)

green()  { printf "\033[32m%s\033[0m\n" "$*"; }
red()    { printf "\033[31m%s\033[0m\n" "$*"; }
yellow() { printf "\033[33m%s\033[0m\n" "$*"; }
hdr()    { printf "\n\033[1m── %s ──\033[0m\n" "$*"; }

fails=0
passes=0
skips=0
pass() { green "  ✓ $*"; passes=$((passes+1)); }
fail() { red   "  ✗ $*"; fails=$((fails+1)); }
skip() { yellow "  ∼ $*"; skips=$((skips+1)); }

TMP=$(mktemp -d -t pgbx-install-test.XXXXXX)
trap 'rm -rf "$TMP"' EXIT

SKILL_NAME="pgbx-skill"
LINK="$TMP/$SKILL_NAME"

hdr "first install"
if CLAUDE_SKILLS_DIR="$TMP" sh "$SKILL_DIR/install.sh" >/dev/null 2>&1; then
  pass "install.sh exited 0"
else
  fail "install.sh exited non-zero on first run"
fi

if [[ -L "$LINK" ]]; then
  pass "symlink created at $LINK"
else
  fail "expected symlink at $LINK — not found"
fi

target=$(readlink "$LINK" 2>/dev/null || true)
if [[ "$target" == "$SKILL_DIR" ]]; then
  pass "symlink points at $SKILL_DIR"
else
  fail "symlink points at '$target', expected '$SKILL_DIR'"
fi

# Snapshot inode + target so we can tell whether second run recreated it.
first_inode=$(ls -i "$LINK" | awk '{print $1}')

hdr "second install (idempotent)"
if CLAUDE_SKILLS_DIR="$TMP" sh "$SKILL_DIR/install.sh" >/dev/null 2>&1; then
  pass "install.sh exited 0 on re-run"
else
  fail "install.sh exited non-zero on second run"
fi

if [[ -L "$LINK" ]]; then
  pass "symlink still exists"
else
  fail "symlink missing after second install"
fi

target=$(readlink "$LINK" 2>/dev/null || true)
if [[ "$target" == "$SKILL_DIR" ]]; then
  pass "symlink still points at $SKILL_DIR"
else
  fail "symlink now points at '$target'"
fi

# install.sh intentionally rm+ln on every run (to heal stale links), so a new
# inode is expected. We assert existence + correct target, not inode stability.

hdr "uninstall"
if CLAUDE_SKILLS_DIR="$TMP" sh "$SKILL_DIR/uninstall.sh" >/dev/null 2>&1; then
  pass "uninstall.sh exited 0"
else
  fail "uninstall.sh exited non-zero"
fi

if [[ -L "$LINK" || -e "$LINK" ]]; then
  fail "symlink still present after uninstall"
else
  pass "symlink removed"
fi

hdr "second uninstall (idempotent)"
if CLAUDE_SKILLS_DIR="$TMP" sh "$SKILL_DIR/uninstall.sh" >/dev/null 2>&1; then
  pass "uninstall.sh exited 0 on re-run"
else
  fail "uninstall.sh exited non-zero on second run"
fi

hdr "summary"
printf "  pass=%d fail=%d skip=%d\n" "$passes" "$fails" "$skips"
exit "$fails"
