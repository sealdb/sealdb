#!/usr/bin/env bash
# TiDB-shaped wrapper around sealdb-itest.
#   ./run-tests.sh                 MemStorage + TiKV (skip TiKV if PD down)
#   ./run-tests.sh -s memory       MemStorage only
#   ./run-tests.sh -s tikv         TiKV only
#   ./run-tests.sh --require-tikv  fail if PD/TiKV is unreachable
#   ./run-tests.sh -t basic        one suite on both stores
#   ./run-tests.sh -r              record goldens from MemStorage, verify TiKV
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CARGO_BIN="${CARGO_BIN:-$HOME/.rustup/toolchains/1.99.1-aarch64-apple-darwin/bin/cargo}"
export PATH="$(dirname "$CARGO_BIN"):$PATH"
export RUSTC="${RUSTC:-$HOME/.rustup/toolchains/1.99.1-aarch64-apple-darwin/bin/rustc}"
unset CARGO_HOME CARGO_TARGET_DIR RUSTUP_HOME || true

RECORD=0
CASE=""
STORE="both"
REQUIRE_TIKV=0
PD=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    -r) RECORD=1; shift ;;
    -t) CASE="${2:-}"; shift 2 ;;
    -s) STORE="${2:-both}"; shift 2 ;;
    -k|--require-tikv) REQUIRE_TIKV=1; shift ;;
    --pd) PD="${2:-}"; shift 2 ;;
    -h|--help)
      sed -n '2,9p' "$0"
      exit 0
      ;;
    *)
      echo "unknown arg: $1" >&2
      exit 2
      ;;
  esac
done

ARGS=(run -q -p sealdb-integrationtest -- --store "$STORE")
if [[ "$RECORD" -eq 1 ]]; then
  ARGS+=(--record)
fi
if [[ -n "$CASE" ]]; then
  ARGS+=(--case "$CASE")
fi
if [[ "$REQUIRE_TIKV" -eq 1 ]]; then
  ARGS+=(--require-tikv)
fi
if [[ -n "$PD" ]]; then
  ARGS+=(--pd "$PD")
fi

cd "$ROOT"
exec cargo "${ARGS[@]}"
