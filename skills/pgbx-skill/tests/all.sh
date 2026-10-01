#!/usr/bin/env bash
#
# Runs all validators + reports a roll-up. Exit code = sum of individual
# failure counts. bash 3.2 compatible.

set -u -o pipefail

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)

green()  { printf "\033[32m%s\033[0m\n" "$*"; }
red()    { printf "\033[31m%s\033[0m\n" "$*"; }
hdr()    { printf "\n\033[1m══ %s ══\033[0m\n" "$*"; }

total=0
passes=0
fails=0

run_one() {
  local label="$1" script="$2"
  hdr "$label"
  set +e
  sh "$script"
  local rc=$?
  fails=$((fails + rc))
  if [ $rc -eq 0 ]; then
    green ">>> $label: OK"
    passes=$((passes+1))
  else
    red ">>> $label: $rc failure(s)"
  fi
  total=$((total+1))
}

run_one "validate-recipes" "$SCRIPT_DIR/validate-recipes.sh"
run_one "install-test"     "$SCRIPT_DIR/install-test.sh"
run_one "router-stress"    "$SCRIPT_DIR/router-stress.sh"

hdr "overall"
printf "  suites=%d clean=%d total-failures=%d\n" "$total" "$passes" "$fails"
exit "$fails"
