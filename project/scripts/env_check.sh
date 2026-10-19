#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TIDB_ROOT="${TIDB_ROOT:-/home/wslu/work/github/db/pingcap/tidb}"

echo "== toolchain =="
rustc --version || true
cargo --version || true
rustup toolchain list 2>/dev/null || true
go version || true
python3 --version || true

echo
echo "== paths =="
for p in "$ROOT" "$TIDB_ROOT" /home/wslu/work/github/db/pingcap/pd /home/wslu/work/github/db/pingcap/tikv; do
  if [[ -e "$p" ]]; then echo "OK   $p"; else echo "MISS $p"; fi
done

echo
echo "== workspace =="
if cargo metadata --no-deps --format-version 1 >/dev/null 2>/tmp/env_cargo.err; then
  echo "OK   cargo metadata"
else
  echo "FAIL cargo metadata"
  sed 's/^/     /' /tmp/env_cargo.err | head -10
fi

echo
echo "== docker =="
docker --version 2>/dev/null || echo "docker not available"
docker compose version 2>/dev/null || docker-compose --version 2>/dev/null || true

echo
echo "== local cluster images =="
grep -RhoE 'image:\s*\S+' "$ROOT/docker" 2>/dev/null | sort -u || true

echo
echo "== ports =="
for port in 2379 4000 20160; do
  if (command -v ss >/dev/null && ss -ltn 2>/dev/null | grep -q ":$port ") || \
     (command -v lsof >/dev/null && lsof -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1); then
    echo "LISTEN $port"
  else
    echo "free   $port"
  fi
done
