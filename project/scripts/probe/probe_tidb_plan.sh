#!/usr/bin/env bash
# Probe the real TiDB planner without touching the reference checkout.
#
# The TiDB clone (TIDB_ROOT) is a read-only reference: authoritative goldens plus
# source to read. When a golden alone cannot explain *why* TiDB picked a plan or
# an estRows value, this script spins up a throwaway `git worktree` in $TMPDIR,
# injects the probe package + env-gated trace patch from this directory, runs the
# SQL, and removes the worktree. The reference tree stays pristine.
#
# Usage:
#   ./project/scripts/probe/probe_tidb_plan.sh -f /tmp/q.sql
#   TIDB_PROBE=1 ./project/scripts/probe/probe_tidb_plan.sh -f /tmp/q.sql  # also trace estRows internals
#
# SQL file format: statements separated by ";\n", "-- " lines are comments.
# `explain`/`select`/`show` print rows tab-separated; others just execute.
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SEALDB_ROOT=$(cd "$SCRIPT_DIR/../../.." && pwd)

TIDB_ROOT=${TIDB_ROOT:-}
if [[ -z "$TIDB_ROOT" ]]; then
  for cand in \
    "${SEALDB_ROOT}/../pingcap/tidb" \
    "${HOME}/work/github/db/pingcap/tidb" \
    "/Users/wslu/work/github/db/pingcap/tidb"; do
    if [[ -d "${cand}/pkg/planner/core" ]]; then
      TIDB_ROOT=$cand
      break
    fi
  done
fi
[[ -n "$TIDB_ROOT" ]] || { echo "error: set TIDB_ROOT" >&2; exit 1; }

PROBE_FILE=""
TRACE="${TIDB_PROBE:-}"
while [[ $# -gt 0 ]]; do
  case "$1" in
    -f) PROBE_FILE="${2:-}"; shift 2 ;;
    --trace) TRACE=1; shift ;;
    -h|--help) sed -n '2,18p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done
[[ -n "$PROBE_FILE" ]] || { echo "error: -f <sql file> required" >&2; exit 2; }
PROBE_FILE=$(cd "$(dirname "$PROBE_FILE")" && pwd)/$(basename "$PROBE_FILE")

WORK=$(mktemp -d "${TMPDIR:-/tmp}/tidb-probe.XXXXXX")
cleanup() {
  git -C "$TIDB_ROOT" worktree remove --force "$WORK" >/dev/null 2>&1 || rm -rf "$WORK"
}
trap cleanup EXIT

git -C "$TIDB_ROOT" worktree add --detach --quiet "$WORK" HEAD
git -C "$WORK" apply "$SCRIPT_DIR/tidb_plan_trace.patch"
if [[ -n "${TIDB_PLANID_TRACE:-}" ]]; then
  git -C "$WORK" apply "$SCRIPT_DIR/tidb_planid_trace.patch"
fi
mkdir -p "$WORK/zzprobe"
cp "$SCRIPT_DIR/probe_test.go" "$WORK/zzprobe/probe_test.go"

cd "$WORK"
# `-tags=intest` is required by testkit's mock store guard; the trace patch also
# gates the `hack` package's go1.25 ABI assertion so the probe runs on whatever
# local toolchain is installed. Goldens never come from this path.
PROBE_FILE="$PROBE_FILE" TIDB_PROBE="$TRACE" TIDB_PLANID_TRACE="${TIDB_PLANID_TRACE:-}" TIDB_SKIP_ABI_CHECK="${TIDB_SKIP_ABI_CHECK:-1}" \
  go test -tags=intest ./zzprobe -run TestProbe -v -count=1 2>&1 \
  | grep -v -E '^(=== RUN|--- PASS|PASS|ok  )' || true
