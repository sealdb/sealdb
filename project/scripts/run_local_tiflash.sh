#!/usr/bin/env bash
# Generate a minimal TiFlash config and start it against local PD.
#
# Env:
#   TIFLASH_BIN     required (or .tiflash-bin-path)
#   SEALDB_PD       default 127.0.0.1:2379
#   TIFLASH_DATA    default /tmp/sealdb-tiflash-data
#   TIFLASH_CFG     default /tmp/sealdb-tiflash.toml

set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SEALDB_ROOT=$(cd "$SCRIPT_DIR/../.." && pwd)
PD=${SEALDB_PD:-127.0.0.1:2379}
DATA=${TIFLASH_DATA:-/tmp/sealdb-tiflash-data}
CFG=${TIFLASH_CFG:-/tmp/sealdb-tiflash.toml}

if [[ -z "${TIFLASH_BIN:-}" && -f "${SEALDB_ROOT}/.tiflash-bin-path" ]]; then
  TIFLASH_BIN=$(cat "${SEALDB_ROOT}/.tiflash-bin-path")
fi
if [[ -z "${TIFLASH_BIN:-}" || ! -x "${TIFLASH_BIN}" ]]; then
  echo "error: set TIFLASH_BIN to a built tiflash binary" >&2
  exit 1
fi

mkdir -p "$DATA"/{data,tmp,log,proxy}

PROXY_CFG="${DATA}/proxy.toml"
cat >"$PROXY_CFG" <<EOF
log-level = "info"
[server]
addr = "0.0.0.0:20170"
advertise-addr = "127.0.0.1:20170"
status-addr = "0.0.0.0:20181"
advertise-status-addr = "127.0.0.1:20181"
engine-addr = "127.0.0.1:3930"
[storage]
data-dir = "${DATA}/proxy"
[pd]
endpoints = ["${PD}"]
[raftstore]
pd-heartbeat-tick-interval = "5s"
EOF

cat >"$CFG" <<EOF
path = "${DATA}/data"
tmp_path = "${DATA}/tmp"
display_name = "sealdb-tiflash"
capacity = "10737418240"

[flash]
service_addr = "0.0.0.0:3930"
[flash.proxy]
addr = "0.0.0.0:20170"
advertise-addr = "127.0.0.1:20170"
data-dir = "${DATA}/proxy"
config = "${PROXY_CFG}"
log-file = "${DATA}/log/proxy.log"
engine-addr = "127.0.0.1:3930"
status-addr = "0.0.0.0:20181"
advertise-status-addr = "127.0.0.1:20181"

[logger]
count = 5
log = "${DATA}/log/tiflash.log"
errorlog = "${DATA}/log/tiflash_error.log"
size = "200M"
level = "info"

[raft]
pd_addr = "${PD}"
EOF

echo "starting TiFlash: $TIFLASH_BIN --config $CFG (pd=$PD)"
exec "$TIFLASH_BIN" --config "$CFG"
