#!/bin/sh

SKILL_NAME="pgbx-skill"
HOME_DIR="${HOME:-$(cd ~ && pwd)}"

CLAUDE_SKILLS_DIR="${CLAUDE_SKILLS_DIR:-$HOME_DIR/.claude/skills}"
CODEX_SKILLS_DIR="$HOME_DIR/.agents/skills"

SKILL_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)

remove_from() {
  target_dir="$1"
  label="$2"
  dest="$target_dir/$SKILL_NAME"

  if [ -L "$dest" ]; then
    current=$(readlink "$dest")
    if [ "$current" != "$SKILL_DIR" ]; then
      echo "  $label -> $SKILL_NAME points elsewhere ($current), leaving it alone"
      return 0
    fi
    rm "$dest"
    echo "  Removed $label -> $dest"
  elif [ -e "$dest" ]; then
    echo "  $label -> $dest is not a symlink, skipping (never touches real dirs)"
  else
    echo "  $label -> not installed, skipping"
  fi
}

echo ""
echo "Uninstalling $SKILL_NAME..."
echo ""

remove_from "$CLAUDE_SKILLS_DIR" "Claude Code"
remove_from "$CODEX_SKILLS_DIR" "Codex Agent"

echo ""
echo "Done."
