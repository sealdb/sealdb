#!/usr/bin/env bash
# Verify SealDB can talk to a live local PD + TiKV.
# Prerequisite: ./project/scripts/run_local_pd_tikv.sh start (in another terminal)
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"

export NO_PROXY='*'
export no_proxy='*'
unset ALL_PROXY all_proxy HTTP_PROXY HTTPS_PROXY http_proxy https_proxy 2>/dev/null || true

./project/scripts/run_local_pd_tikv.sh status

echo "== sealdb-store live tests =="
cargo test -q -p sealdb-store --lib -- --nocapture \
  current_timestamp_if_available \
  roundtrip_if_available \
  list_stores_and_regions \
  plan_tikv_batch_if_pd \
  split_key_ranges_via_pd

echo "OK: PD/TiKV link verified"
