#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TIDB_ROOT="${TIDB_ROOT:-/home/wslu/work/github/db/pingcap/tidb}"
PHASE="${1:-}"
REPORT_DIR="$ROOT/project/reports/phase-${PHASE}"

usage() {
  echo "usage: TIDB_ROOT=/path/to/tidb $0 <phase-number>   e.g. 02"
}

if [[ -z "$PHASE" ]]; then
  usage
  exit 2
fi

case "$PHASE" in
  00) REQUIRE=""; SUITE="parser-smoke" ;;
  01) REQUIRE="pkg/types,pkg/util/codec,pkg/tablecodec,pkg/kv,pkg/parser/model,pkg/parser/mysql,pkg/parser/types"; SUITE="codec" ;;
  02) REQUIRE="pkg/parser,pkg/session,pkg/planner,pkg/executor,cmd/tidb-server"; SUITE="parser-core" ;;
  03) REQUIRE="pkg/meta,pkg/infoschema,pkg/domain,pkg/session"; SUITE="metadata" ;;
  04) REQUIRE="pkg/planner,pkg/expression"; SUITE="plan-single-table" ;;
  05) REQUIRE="pkg/executor,pkg/distsql,pkg/store,pkg/expression,pkg/util/chunk"; SUITE="exec-single-table" ;;
  06) REQUIRE="pkg/sessiontxn,pkg/store/driver,pkg/kv,pkg/session"; SUITE="txn-basic" ;;
  07) REQUIRE="pkg/statistics,pkg/planner"; SUITE="plan-cbo" ;;
  08) REQUIRE="pkg/ddl,pkg/privilege,pkg/bindinfo"; SUITE="ddl-basic" ;;
  09) REQUIRE="pkg/server,cmd/tidb-server"; SUITE="protocol" ;;
  *) echo "unknown phase: $PHASE" >&2; exit 2 ;;
esac

mkdir -p "$REPORT_DIR"
STATUS=0

run() {
  local name="$1"; shift
  echo "== $name =="
  if "$@" >"$REPORT_DIR/${name}.log" 2>&1; then
    echo "PASS: $name"
  else
    echo "FAIL: $name (see $REPORT_DIR/${name}.log)"
    STATUS=1
  fi
}

run fmt cargo fmt --all --check
run clippy cargo clippy --workspace --all-targets -- -D warnings
run test cargo test --workspace

if [[ -n "$REQUIRE" ]]; then
  run parity python3 "$ROOT/project/scripts/parity_check.py" \
    --tidb-root "$TIDB_ROOT" \
    --rust-root "$ROOT" \
    --require "$REQUIRE" \
    --strict-headers \
    --json "$REPORT_DIR/parity.json"
else
  run parity python3 "$ROOT/project/scripts/parity_check.py" \
    --tidb-root "$TIDB_ROOT" \
    --rust-root "$ROOT" \
    --strict-headers \
    --json "$REPORT_DIR/parity.json"
fi

run oracle "$ROOT/project/scripts/oracle_diff.sh" --suite "$SUITE" --out "$REPORT_DIR/oracle.json"

cat >"$REPORT_DIR/summary.md" <<EOF
# Phase $PHASE Gate Report

- Date: $(date -u +%Y-%m-%dT%H:%M:%SZ)
- Rust: $(rustc --version 2>/dev/null || echo unknown)
- TiDB root: $TIDB_ROOT
- Suite: $SUITE

## Result

$(for f in "$REPORT_DIR"/*.log; do
  name="$(basename "$f" .log)"
  echo "- $name: see $f"
done)

See individual logs and parity.json/oracle.json.

## Conclusion

$(if [[ "$STATUS" -eq 0 ]]; then echo "PASS"; else echo "FAIL"; fi)
EOF

echo
echo "report: $REPORT_DIR/summary.md"
exit "$STATUS"
