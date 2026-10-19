#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ORACLE="$ROOT/tools/oracle"
SUITE=""
SQL=""
MODE="exec"
OUT="$ROOT/project/reports/oracle-latest.json"

usage() {
  cat <<'EOF'
usage: oracle_diff.sh --suite <name> [--out FILE]
       oracle_diff.sh --sql "SELECT ..." --mode ast|plan|exec|error|txn [--out FILE]

Requires tools/oracle (Go module referencing the local TiDB checkout).
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --suite) SUITE="$2"; shift 2 ;;
    --sql) SQL="$2"; shift 2 ;;
    --mode) MODE="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage; exit 2 ;;
  esac
done

if [[ ! -d "$ORACLE" ]]; then
  echo "error: $ORACLE not found (Phase 0 deliverable)" >&2
  exit 2
fi

mkdir -p "$(dirname "$OUT")"

if [[ -n "$SUITE" ]]; then
  echo "oracle suite: $SUITE"
  (cd "$ORACLE" && go run ./cmd/tidb-oracle run --suite "$SUITE" --out "$OUT")
elif [[ -n "$SQL" ]]; then
  echo "oracle mode: $MODE"
  (cd "$ORACLE" && go run ./cmd/tidb-oracle "$MODE" --sql "$SQL" --out "$OUT")
else
  usage
  exit 2
fi

echo "report: $OUT"
