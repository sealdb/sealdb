#!/usr/bin/env bash
# Run a regression wave/suite and write the first failure into project/regression/waves/.
#
#   ./project/scripts/triage_itest.sh --wave R1 --status in_progress
#   ./project/scripts/triage_itest.sh -t select --status in_progress
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
WAVE=""
STATUS="in_progress"
CASE=""
STORE="memory"
OUT_DIR="$ROOT/project/regression/waves"
mkdir -p "$OUT_DIR"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --wave) WAVE="${2:-}"; shift 2 ;;
    --status) STATUS="${2:-}"; shift 2 ;;
    -t) CASE="${2:-}"; shift 2 ;;
    -s) STORE="${2:-}"; shift 2 ;;
    *) echo "unknown $1" >&2; exit 2 ;;
  esac
done

ARGS=(-s "$STORE" --status "$STATUS")
[[ -n "$WAVE" ]] && ARGS+=(--wave "$WAVE")
[[ -n "$CASE" ]] && ARGS+=(-t "$CASE")

LABEL=${CASE:-${WAVE:-all}}
LABEL=${LABEL//\//_}
OUT="$OUT_DIR/${LABEL}-triage.txt"

set +e
(cd "$ROOT" && ./tests/integrationtest/run-tests.sh "${ARGS[@]}") >"$OUT" 2>&1
CODE=$?
set -e

echo "exit=$CODE" | tee -a "$OUT"
echo "wrote $OUT"
exit 0
