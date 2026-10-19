#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ORACLE="$ROOT/tools/oracle"
SUITE=""
SQL=""
OUT="$ROOT/project/reports/oracle-latest.json"

usage() {
  cat <<'EOF'
usage: oracle_diff.sh --suite <name> [--out FILE]
       oracle_diff.sh --sql "SELECT ..." [--out FILE]

Suite names map to tools/oracle/testdata/<name>/*.sql.
Requires the Go oracle module.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --suite) SUITE="$2"; shift 2 ;;
    --sql) SQL="$2"; shift 2 ;;
    --mode) shift 2 ;;
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
export GOPROXY="${GOPROXY:-https://goproxy.cn,direct}"
export GOTOOLCHAIN="${GOTOOLCHAIN:-auto}"

if [[ -n "$SUITE" ]]; then
  DIR="$ORACLE/testdata/$SUITE"
  if [[ ! -d "$DIR" ]]; then
    echo "error: suite not found: $DIR" >&2
    exit 2
  fi
  (cd "$ORACLE" && go run ./cmd/tidb-oracle -suite "testdata/$SUITE" -pretty -out "$OUT")
elif [[ -n "$SQL" ]]; then
  (cd "$ORACLE" && go run ./cmd/tidb-oracle -sql "$SQL" -pretty -out "$OUT")
else
  usage
  exit 2
fi

echo "report: $OUT"
