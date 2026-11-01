#!/usr/bin/env bash
# TiDB-shaped wrapper around sealdb-itest.
#   ./run-tests.sh              compare t/*.test to r/*.result
#   ./run-tests.sh -t basic     one suite
#   ./run-tests.sh -r           record all suites
#   ./run-tests.sh -r -t basic  record one suite
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CARGO_BIN="${CARGO_BIN:-$HOME/.rustup/toolchains/1.99.1-aarch64-apple-darwin/bin/cargo}"
export PATH="$(dirname "$CARGO_BIN"):$PATH"
export RUSTC="${RUSTC:-$HOME/.rustup/toolchains/1.99.1-aarch64-apple-darwin/bin/rustc}"
unset CARGO_HOME CARGO_TARGET_DIR RUSTUP_HOME || true

RECORD=0
CASE=""

while getopts "hrt:" opt; do
  case "$opt" in
    r) RECORD=1 ;;
    t) CASE="$OPTARG" ;;
    h)
      sed -n '2,7p' "$0"
      exit 0
      ;;
    *) exit 2 ;;
  esac
done

ARGS=(run -q -p sealdb-integrationtest --)
if [[ "$RECORD" -eq 1 ]]; then
  ARGS+=(--record)
fi
if [[ -n "$CASE" ]]; then
  ARGS+=(--case "$CASE")
fi

cd "$ROOT"
exec cargo "${ARGS[@]}"
