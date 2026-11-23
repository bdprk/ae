#!/bin/bash
# Cloud-gate leg (P1.1 verification): run the full Linux gate suite on a
# clean clone, on a real Linux host (e.g. an InstaBox sandbox), and keep the
# raw tails in the log so a compile failure cannot look like "no output".
#
# Usage: scripts/cloud_gate.sh [log-path]
#   ROUTE_MAX  the route-report ratchet ceiling (default 310)
#
# The caller must build first, and the `ae` binary must exist at
# target/debug/ae before route_report/deny_route/verify run; building here is
# intentional (a fresh clone has no binary).
set -u
cd "$(dirname "$0")/.." || exit 2
# shellcheck disable=SC1091
source "$HOME/.cargo/env" 2>/dev/null || true
LOG=${1:-/tmp/ae-cloud-gate.log}
: > "$LOG"
{
  date -u
  git log --oneline -1
  echo "== cargo build"
  cargo build --workspace --locked 2>&1 | tail -2
  echo "== cargo test"
  cargo test --workspace --locked 2>&1 | tail -20
  echo "== script unit tests"
  python3 -m unittest discover -s scripts -p "test_*.py" 2>&1 | tail -2
  echo "== std layering"
  python3 scripts/std_layering.py
  echo "== route report"
  python3 scripts/route_report.py --max "${ROUTE_MAX:-310}" 2>&1 | tail -3
  echo "== deny manifest"
  python3 scripts/deny_route.py 2>&1 | tail -2
  echo "== generated C (sanitizers)"
  python3 scripts/verify_generated_c.py --sanitize 2>&1 | tail -3
  echo "== 50k mutation gate"
  cargo test -p ae-compiler --lib mutated_sources_never_panic_50k -- --ignored 2>&1 | tail -2
  date -u
  echo CLOUD_GATE_DONE
} > "$LOG" 2>&1