#!/usr/bin/env bash
# Build TiFlash from a local checkout for SealDB C6 cluster e2e.
#
# Usage:
#   TIFLASH_SRC=/path/to/tiflash ./project/scripts/build_tiflash.sh
# Defaults:
#   TIFLASH_SRC=$HOME/work/github/db/pingcap/tiflash (or sibling of sealdb)
#   PRESET=release   (RELWITHDEBINFO, tests off — faster than debug)
#
# Requires: CMake ≥ 3.23, Ninja, AppleClang 14+ / Clang 17+, Rust, ccache (optional).

set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SEALDB_ROOT=$(cd "$SCRIPT_DIR/../.." && pwd)

if [[ -z "${TIFLASH_SRC:-}" ]]; then
  for cand in \
    "${SEALDB_ROOT}/../pingcap/tiflash" \
    "${HOME}/work/github/db/pingcap/tiflash" \
    "/Users/wslu/work/github/db/pingcap/tiflash"; do
    if [[ -f "${cand}/CMakeLists.txt" ]]; then
      TIFLASH_SRC=$cand
      break
    fi
  done
fi

if [[ -z "${TIFLASH_SRC:-}" || ! -f "${TIFLASH_SRC}/CMakeLists.txt" ]]; then
  echo "error: set TIFLASH_SRC to a TiFlash checkout" >&2
  exit 1
fi

PRESET=${PRESET:-release}
# Prefer Homebrew cmake (≥3.23); system cmake on some Macs is too old.
export PATH="/opt/homebrew/bin:/usr/local/bin:${PATH}"

CMAKE=$(command -v cmake)
CMAKE_VER=$("$CMAKE" --version | head -1 | awk '{print $3}')
echo "using cmake=$CMAKE ($CMAKE_VER) preset=$PRESET src=$TIFLASH_SRC"

cd "$TIFLASH_SRC"

if [[ ! -f contrib/tiflash-proxy/Cargo.toml ]]; then
  echo "initializing git submodules (one-time, large)…"
  git submodule update --init --recursive -j "$(sysctl -n hw.ncpu 2>/dev/null || echo 8)"
fi

cmake --preset "$PRESET"
cmake --build --preset "$PRESET" --target tiflash

BIN="${TIFLASH_SRC}/cmake-build-${PRESET}/dbms/src/Server/tiflash"
# release preset binaryDir is cmake-build-release
if [[ "$PRESET" == "release" ]]; then
  BIN="${TIFLASH_SRC}/cmake-build-release/dbms/src/Server/tiflash"
elif [[ "$PRESET" == "dev" ]]; then
  BIN="${TIFLASH_SRC}/cmake-build-debug/dbms/src/Server/tiflash"
fi

if [[ ! -x "$BIN" ]]; then
  # Fallback: find freshly built binary
  BIN=$(find "$TIFLASH_SRC" -path '*/dbms/src/Server/tiflash' -type f -perm +111 2>/dev/null | head -1 || true)
fi

if [[ -z "$BIN" || ! -x "$BIN" ]]; then
  echo "error: tiflash binary not found after build" >&2
  exit 1
fi

echo "OK: $BIN"
"$BIN" version 2>/dev/null || "$BIN" --version 2>/dev/null || true
echo "$BIN" >"${SEALDB_ROOT}/.tiflash-bin-path"
