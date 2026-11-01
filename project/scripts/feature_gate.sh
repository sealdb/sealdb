#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TIDB_ROOT="${TIDB_ROOT:-/Users/wslu/work/github/db/pingcap/tidb}"
if [[ ! -d "$TIDB_ROOT" && -d /home/wslu/work/github/db/pingcap/tidb ]]; then
  TIDB_ROOT=/home/wslu/work/github/db/pingcap/tidb
fi
FEATURE="${1:-}"
REPORT_DIR="$ROOT/project/reports/features/${FEATURE}"
CARGO_BIN="${CARGO_BIN:-$HOME/.rustup/toolchains/1.99.1-aarch64-apple-darwin/bin/cargo}"
export PATH="$(dirname "$CARGO_BIN"):$PATH"
export RUSTC="${RUSTC:-$HOME/.rustup/toolchains/1.99.1-aarch64-apple-darwin/bin/rustc}"

usage() {
  echo "usage: TIDB_ROOT=/path/to/tidb $0 <FEATURE-ID>   e.g. F01"
}

if [[ -z "$FEATURE" ]]; then
  usage
  exit 2
fi

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
run parity python3 "$ROOT/project/scripts/parity_check.py" \
  --tidb-root "$TIDB_ROOT" \
  --rust-root "$ROOT" \
  --strict \
  --json "$REPORT_DIR/parity.json"

if [[ -d "$ROOT/tools/oracle/testdata/${FEATURE}" ]]; then
  run oracle-suite "$ROOT/project/scripts/oracle_diff.sh" --suite "$FEATURE" --out "$REPORT_DIR/oracle.json"
fi

case "$FEATURE" in
  U3|F01)
    run execute-select1 cargo run -q -p sealdb-server-bin -- --execute "SELECT 1"
    ;;
  F02)
    run execute-createdb cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1"
    ;;
esac

if [[ ! -f "$REPORT_DIR/corpus.lock.json" ]]; then
  echo "FAIL: missing $REPORT_DIR/corpus.lock.json"
  STATUS=1
fi
if [[ ! -f "$REPORT_DIR/gaps.md" ]]; then
  echo "FAIL: missing $REPORT_DIR/gaps.md"
  STATUS=1
fi

cat >"$REPORT_DIR/summary.md" <<EOF
# Feature $FEATURE Gate Report

- Date: $(date -u +%Y-%m-%dT%H:%M:%SZ)
- Rust: $(rustc --version 2>/dev/null || echo unknown)
- TiDB root: $TIDB_ROOT

## Conclusion

$(if [[ "$STATUS" -eq 0 ]]; then echo "PASS"; else echo "FAIL"; fi)
EOF

echo
echo "report: $REPORT_DIR/summary.md"
exit "$STATUS"
