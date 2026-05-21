#!/usr/bin/env bash
set -euo pipefail

# CI smoke test for non-interactive just recipes.
# Defaults to a "full" pass; use --quick to run a shorter gate.

MODE="full"
if [[ "${1:-}" == "--quick" ]]; then
  MODE="quick"
fi

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"

run() {
  echo
  echo "==> $*"
  "$@"
}

cleanup() {
  set +e
  just uc-full-down >/dev/null 2>&1 || true
  just down >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "Running CI smoke (${MODE}) from ${ROOT_DIR}"

# Ensure we always start from a known state.
run just down || true
run just clean || true
run just uc-full-down || true

run just init
run just up
run just health

if [[ "${MODE}" == "quick" ]]; then
  run just demo
  run just run "cargo --version"
  run just uc-oss-up
  run just uc-oss-down
  run just down
  echo
  echo "CI smoke (quick) passed."
  exit 0
fi

run just demo
run just demo-analyst
run just demo-physician
run just demo-admin
run just demo-ssn
run just compile
run just run "cargo --version"
run just test
run just uc-oss-up
run just uc-oss-down
run just uc-full-up
run just uc-full-demo
run just uc-full-down
run just down

echo
echo "CI smoke (full) passed."
