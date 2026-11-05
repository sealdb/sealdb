#!/usr/bin/env bash
# Record / refresh integrationtest goldens from a real TiDB binary (authoritative).
#
# Usage:
#   ./project/scripts/record_tidb_itest.sh                 # verify all non-deferred
#   ./project/scripts/record_tidb_itest.sh -t select       # record one suite
#   ./project/scripts/record_tidb_itest.sh -t select -r    # force TiDB -r record
#   TIDB_BIN=/path/to/tidb-server ./project/scripts/record_tidb_itest.sh -t show
#
# Goldens are written into tests/integrationtest/r/ (SealDB tree).
# Never use SealDB sealdb-itest --record for TiDB corpus suites.
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SEALDB_ROOT=$(cd "$SCRIPT_DIR/../.." && pwd)
ITEST="$SEALDB_ROOT/tests/integrationtest"

TIDB_ROOT=${TIDB_ROOT:-}
if [[ -z "$TIDB_ROOT" ]]; then
  for cand in \
    "${SEALDB_ROOT}/../pingcap/tidb" \
    "${HOME}/work/github/db/pingcap/tidb" \
    "/Users/wslu/work/github/db/pingcap/tidb"; do
    if [[ -d "${cand}/tests/integrationtest" ]]; then
      TIDB_ROOT=$cand
      break
    fi
  done
fi

if [[ -z "${TIDB_ROOT:-}" ]]; then
  echo "error: set TIDB_ROOT" >&2
  exit 1
fi

TIDB_ITEST="$TIDB_ROOT/tests/integrationtest"
CASE=""
DO_RECORD=0
BUILD=y

while [[ $# -gt 0 ]]; do
  case "$1" in
    -t) CASE="${2:-}"; shift 2 ;;
    -r) DO_RECORD=1; shift ;;
    -b) BUILD="${2:-y}"; shift 2 ;;
    --tidb-bin) TIDB_BIN="${2:-}"; shift 2 ;;
    -h|--help)
      sed -n '2,12p' "$0"
      exit 0
      ;;
    *)
      echo "unknown arg: $1" >&2
      exit 2
      ;;
  esac
done

# Prefer an already-built binary; otherwise let TiDB run-tests.sh build.
TIDB_BIN=${TIDB_BIN:-}
if [[ -z "$TIDB_BIN" ]]; then
  for cand in \
    "$TIDB_ROOT/bin/tidb-server" \
    "$TIDB_ITEST/integrationtest_tidb-server" \
    "$(command -v tidb-server 2>/dev/null || true)"; do
    if [[ -n "$cand" && -x "$cand" ]]; then
      TIDB_BIN=$cand
      break
    fi
  done
fi

echo "TIDB_ROOT=$TIDB_ROOT"
echo "ITEST(sealdb)=$ITEST"
echo "CASE=${CASE:-all} record=$DO_RECORD"

# Work inside a temp copy that points t/r/s at SealDB corpus via bind-style sync.
WORK=$(mktemp -d /tmp/sealdb-tidb-itest.XXXXXX)
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

mkdir -p "$WORK"
# Use SealDB's imported t/r/s so record writes land in our tree after copy-back.
rsync -a "$ITEST/t/" "$WORK/t/"
rsync -a "$ITEST/r/" "$WORK/r/"
if [[ -d "$ITEST/s" ]]; then
  rsync -a "$ITEST/s/" "$WORK/s/"
fi
for f in config.toml disable_new_collation.toml run-tests.sh; do
  if [[ -f "$TIDB_ITEST/$f" ]]; then
    cp "$TIDB_ITEST/$f" "$WORK/$f"
  fi
done
chmod +x "$WORK/run-tests.sh" 2>/dev/null || true

cd "$WORK"
ARGS=()
if [[ -n "$TIDB_BIN" ]]; then
  ARGS+=(-s "$TIDB_BIN" -b n)
else
  ARGS+=(-b "$BUILD")
fi

if [[ "$DO_RECORD" -eq 1 ]]; then
  if [[ -n "$CASE" ]]; then
    ARGS+=(-r "$CASE")
  else
    ARGS+=(-r all)
  fi
else
  if [[ -n "$CASE" ]]; then
    ARGS+=(-t "$CASE")
  fi
fi

echo "+ ./run-tests.sh ${ARGS[*]}"
if ! ./run-tests.sh "${ARGS[@]}"; then
  echo "error: TiDB integrationtest failed" >&2
  exit 1
fi

# Copy results (and any refreshed stats) back into SealDB.
rsync -a "$WORK/r/" "$ITEST/r/"
if [[ -d "$WORK/s" ]]; then
  rsync -a "$WORK/s/" "$ITEST/s/"
fi

# Stamp provenance for recorded suites.
PROV="$SEALDB_ROOT/project/regression/golden_provenance.jsonl"
mkdir -p "$(dirname "$PROV")"
{
  echo -n "{\"at\":\"$(date -u +%Y-%m-%dT%H:%M:%SZ)\",\"tidb_root\":$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "$TIDB_ROOT")"
  echo -n ",\"tidb_bin\":$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "${TIDB_BIN:-built-by-run-tests}")"
  echo -n ",\"case\":$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "${CASE:-all}")"
  echo -n ",\"record\":$DO_RECORD}"
  echo
} >>"$PROV"

echo "OK: goldens synced to $ITEST/r (provenance -> $PROV)"
