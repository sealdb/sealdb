#!/usr/bin/env bash
# Start a single-node PD + TiKV for SealDB local verification.
# Uses tiup-installed binaries (default v8.5.0).
#
# Env:
#   PD_BIN / TIKV_BIN  override binary paths
#   SEALDB_CLUSTER_DATA  default /tmp/sealdb-pd-tikv
#   SEALDB_PD_VERSION    default v8.5.0
#
# Usage:
#   ./project/scripts/run_local_pd_tikv.sh          # start (foreground wait)
#   ./project/scripts/run_local_pd_tikv.sh stop
#   ./project/scripts/run_local_pd_tikv.sh status

set -euo pipefail

# Local PD/TiKV must not go through HTTP/SOCKS proxies (Clash etc. break gRPC).
export NO_PROXY="127.0.0.1,localhost,::1,${NO_PROXY:-}"
export no_proxy="$NO_PROXY"
unset ALL_PROXY all_proxy HTTP_PROXY HTTPS_PROXY http_proxy https_proxy 2>/dev/null || true

VER=${SEALDB_PD_VERSION:-v8.5.0}
TIUP_HOME=${TIUP_HOME:-$HOME/.tiup}
PD_BIN=${PD_BIN:-$TIUP_HOME/components/pd/$VER/pd-server}
TIKV_BIN=${TIKV_BIN:-$TIUP_HOME/components/tikv/$VER/tikv-server}
DATA=${SEALDB_CLUSTER_DATA:-/tmp/sealdb-pd-tikv}
CMD=${1:-start}

mkdir -p "$DATA"/{pd,tikv,log}

status() {
  local ok=0
  if curl -sf --noproxy '*' http://127.0.0.1:2379/pd/api/v1/version >/dev/null; then
    echo "PD: up ($(curl -sf --noproxy '*' http://127.0.0.1:2379/pd/api/v1/version))"
  else
    echo "PD: down"; ok=1
  fi
  if curl -sf --noproxy '*' http://127.0.0.1:2379/pd/api/v1/stores >/dev/null; then
    curl -sf --noproxy '*' http://127.0.0.1:2379/pd/api/v1/stores \
      | python3 -c 'import json,sys
d=json.load(sys.stdin)
stores=d.get("stores") or []
print("stores:", len(stores))
for s in stores:
  st=s.get("store") or {}
  print(" ", st.get("address"), "state="+str(st.get("state_name")))'
  else
    echo "stores: unavailable"; ok=1
  fi
  return $ok
}

stop() {
  if [[ -f "$DATA/pd.pid" ]]; then kill "$(cat "$DATA/pd.pid")" 2>/dev/null || true; fi
  if [[ -f "$DATA/tikv.pid" ]]; then kill "$(cat "$DATA/tikv.pid")" 2>/dev/null || true; fi
  pkill -f "$DATA/pd" 2>/dev/null || true
  pkill -f "$DATA/tikv" 2>/dev/null || true
  echo "stopped"
}

start() {
  if [[ ! -x "$PD_BIN" || ! -x "$TIKV_BIN" ]]; then
    echo "error: missing binaries" >&2
    echo "  PD_BIN=$PD_BIN" >&2
    echo "  TIKV_BIN=$TIKV_BIN" >&2
    echo "hint: tiup install pd:$VER tikv:$VER" >&2
    exit 1
  fi
  stop >/dev/null 2>&1 || true
  sleep 1

  "$PD_BIN" \
    --name=pd1 \
    --data-dir="$DATA/pd" \
    --client-urls=http://127.0.0.1:2379 \
    --peer-urls=http://127.0.0.1:2380 \
    --advertise-client-urls=http://127.0.0.1:2379 \
    --advertise-peer-urls=http://127.0.0.1:2380 \
    --log-file="$DATA/log/pd.log" \
    >"$DATA/log/pd.stdout" 2>&1 &
  echo $! >"$DATA/pd.pid"

  for _ in $(seq 1 40); do
    if curl -sf --noproxy '*' http://127.0.0.1:2379/pd/api/v1/version >/dev/null; then
      break
    fi
    sleep 0.25
  done
  if ! curl -sf --noproxy '*' http://127.0.0.1:2379/pd/api/v1/version >/dev/null; then
    echo "error: PD failed to start; see $DATA/log/pd.log" >&2
    exit 1
  fi

  "$TIKV_BIN" \
    --addr=127.0.0.1:20160 \
    --advertise-addr=127.0.0.1:20160 \
    --status-addr=127.0.0.1:20180 \
    --pd=http://127.0.0.1:2379 \
    --data-dir="$DATA/tikv" \
    --log-file="$DATA/log/tikv.log" \
    >"$DATA/log/tikv.stdout" 2>&1 &
  echo $! >"$DATA/tikv.pid"

  for _ in $(seq 1 60); do
    state=$(curl -sf --noproxy '*' http://127.0.0.1:2379/pd/api/v1/stores \
      | python3 -c 'import json,sys; d=json.load(sys.stdin); print((d.get("stores") or [{}])[0].get("store",{}).get("state_name",""))' 2>/dev/null || true)
    if [[ "$state" == "Up" ]]; then
      status
      echo "OK: cluster ready (pids pd=$(cat "$DATA/pd.pid") tikv=$(cat "$DATA/tikv.pid"))"
      # Keep script alive so callers that background this keep children.
      wait
      return 0
    fi
    sleep 0.5
  done
  echo "error: TiKV store not Up; see $DATA/log/tikv.log" >&2
  status || true
  exit 1
}

case "$CMD" in
  start) start ;;
  stop) stop ;;
  status) status ;;
  *) echo "usage: $0 [start|stop|status]" >&2; exit 2 ;;
esac
