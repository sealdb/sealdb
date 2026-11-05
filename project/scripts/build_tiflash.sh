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
# Prefer Homebrew cmake (≥3.23) and LLVM (CLT libc++ is often incomplete).
export PATH="/opt/homebrew/opt/llvm/bin:/opt/homebrew/bin:/usr/local/bin:${PATH}"
if [[ -x /opt/homebrew/opt/llvm/bin/clang ]]; then
  export CC=${CC:-/opt/homebrew/opt/llvm/bin/clang}
  export CXX=${CXX:-/opt/homebrew/opt/llvm/bin/clang++}
fi

CMAKE=$(command -v cmake)
CMAKE_VER=$("$CMAKE" --version | head -1 | awk '{print $3}')
echo "using cmake=$CMAKE ($CMAKE_VER) cxx=${CXX:-default} preset=$PRESET src=$TIFLASH_SRC"

cd "$TIFLASH_SRC"

if [[ ! -f contrib/tiflash-proxy/Cargo.toml ]]; then
  echo "initializing git submodules (one-time, large)…"
  git submodule update --init --recursive -j "$(sysctl -n hw.ncpu 2>/dev/null || echo 8)"
fi
# Nested deps that --recursive sometimes skips after transient SSL failures.
git submodule update --init --recursive contrib/usearch contrib/prometheus-cpp 2>/dev/null || true
if [[ -d contrib/grpc ]]; then
  git -C contrib/grpc submodule update --init third_party/cares/cares third_party/protobuf \
    third_party/abseil-cpp third_party/boringssl-with-bazel third_party/googleapis third_party/xds \
    2>/dev/null || true
fi

BUILD_DIR="cmake-build-${PRESET}"
if [[ "$PRESET" == "release" ]]; then BUILD_DIR=cmake-build-release; fi
if [[ "$PRESET" == "dev" ]]; then BUILD_DIR=cmake-build-debug; fi

if [[ ! -f "${BUILD_DIR}/build.ninja" ]]; then
  cmake -S . -B "$BUILD_DIR" -GNinja \
    -DCMAKE_BUILD_TYPE=$([[ "$PRESET" == "dev" ]] && echo DEBUG || echo RELWITHDEBINFO) \
    -DENABLE_TESTS=$([[ "$PRESET" == "dev" ]] && echo ON || echo OFF) \
    ${CC:+-DCMAKE_C_COMPILER="$CC"} \
    ${CXX:+-DCMAKE_CXX_COMPILER="$CXX"}
fi

# Homebrew LLVM 22 + C++23: Poco needs explicit <system_error> includes.
for f in \
  contrib/poco/Foundation/src/Event.cpp \
  contrib/poco/Foundation/src/Thread_STD.cpp; do
  if [[ -f "$f" ]] && ! grep -q '#include <system_error>' "$f"; then
    # Insert after the last Poco include block's first include line region.
    perl -i -0pe 's/(#include "Poco\/[^\"]+"\n)/$1#include <system_error>\n/s' "$f" || true
  fi
done

JOBS=${JOBS:-$(sysctl -n hw.ncpu 2>/dev/null || echo 8)}
if [[ "$JOBS" -gt 8 ]]; then JOBS=8; fi
cmake --build "$BUILD_DIR" --target tiflash -j"$JOBS"

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
