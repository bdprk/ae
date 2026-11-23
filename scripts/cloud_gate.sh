#!/bin/bash
# Cloud-gate leg (P1.1 verification): run the full Linux gate suite on a
# clean clone, on a real Linux host (e.g. an InstaBox sandbox), and keep each
# section's verdict line (plus the failure lines when a section fails) in the
# log, so a hidden failure cannot look like a green run.
#
# Usage: scripts/cloud_gate.sh [log-path]
#   ROUTE_MAX  the route-report ratchet ceiling (default 17)
#
# The script builds first: a fresh clone has no `ae` binary, and
# route_report/deny_route/verify need it at the target dir.
set -u
cd "$(dirname "$0")/.." || exit 2
# shellcheck disable=SC1091
source "$HOME/.cargo/env" 2>/dev/null || true
LOG=${1:-/tmp/ae-cloud-gate.log}
STEP=/tmp/ae-cloud-step.log
VERDICT=0
step() {
  local name=$1
  shift
  if "$@" > "$STEP" 2>&1; then
    echo "ok: $name"
  else
    echo "FAILED: $name"
    VERDICT=1
  fi
}
{
  date -u
  git log --oneline -1
  echo "== cargo build"
  step "cargo build" cargo build --workspace --locked
  echo "== cargo test"
  step "cargo test" cargo test --workspace --locked
  grep -E "^test result:" "$STEP" | tail -6
  grep -E "^(error|thread '.*' panicked)" "$STEP" | head -10
  echo "== script unit tests"
  step "script tests" python3 -m unittest discover -s scripts -p "test_*.py"
  grep -E "^(FAILED|OK)" "$STEP" | tail -2
  echo "== std layering"
  step "std layering" python3 scripts/std_layering.py
  tail -2 "$STEP"
  echo "== route report"
  step "route report" python3 scripts/route_report.py --max "${ROUTE_MAX:-17}"
  grep -E "fallbacks=|check skipped \(fails standalone\)" "$STEP" | tail -5
  echo "== deny manifest"
  step "deny manifest" python3 scripts/deny_route.py
  grep -E "deny-legacy:" "$STEP" | tail -1
  echo "== generated C (sanitizers)"
  step "generated C" python3 scripts/verify_generated_c.py --sanitize
  grep -E "^(PASS|FAIL)" "$STEP" | tail -4
  echo "== 50k mutation gate"
  step "50k mutation gate" cargo test -p ae-compiler --lib mutated_sources_never_panic_50k -- --ignored
  grep -E "^test result:" "$STEP" | tail -1
  date -u
  if [ "$VERDICT" -ne 0 ]; then
    echo "CLOUD_GATE_FAILED"
  else
    echo "CLOUD_GATE_DONE"
  fi
} > "$LOG" 2>&1