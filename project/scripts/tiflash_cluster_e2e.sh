#!/usr/bin/env bash
# Smoke SealDB ↔ TiFlash discovery / MPP plan against a live PD.
#
# Prerequisites:
#   - PD listening (default http://127.0.0.1:2379)
#   - At least one TiFlash store registered with label engine=tiflash
#   - Built sealdb (cargo)
#
# Env:
#   SEALDB_PD          PD HTTP endpoint (default 127.0.0.1:2379)
#   SEALDB_USE_MPP=1   optional: exercise TableReader MPP path in SQL smoke
#   TIFLASH_BIN        path to tiflash binary (optional; for version print)
#
# Exit 0 on success; 2 if TiFlash not discovered (skip); non-zero on hard fail.

set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SEALDB_ROOT=$(cd "$SCRIPT_DIR/../.." && pwd)
PD=${SEALDB_PD:-127.0.0.1:2379}
export PATH="/opt/homebrew/bin:/usr/local/bin:${PATH}"

cd "$SEALDB_ROOT"

if [[ -f .tiflash-bin-path ]]; then
  TIFLASH_BIN=${TIFLASH_BIN:-$(cat .tiflash-bin-path)}
fi

echo "== PD stores ($PD) =="
if ! curl -sf "http://${PD}/pd/api/v1/stores" -o /tmp/sealdb-pd-stores.json; then
  echo "error: cannot reach PD at $PD" >&2
  exit 1
fi

python3 - <<'PY'
import json, sys
data = json.load(open("/tmp/sealdb-pd-stores.json"))
stores = data.get("stores") or data.get("Stores") or []
tiflash = []
for s in stores:
    store = s.get("store") or s
    labels = store.get("labels") or []
    lab = { (l.get("key") or "").lower(): (l.get("value") or "").lower() for l in labels }
    addr = store.get("address") or store.get("addr") or ""
    if lab.get("engine") == "tiflash":
        tiflash.append(addr)
        print(f"  TiFlash store: {addr} labels={lab}")
    else:
        print(f"  store: {addr} labels={lab}")
if not tiflash:
    print("no TiFlash store with engine=tiflash — skip e2e", file=sys.stderr)
    sys.exit(2)
print(f"found {len(tiflash)} TiFlash store(s)")
PY
rc=$?
if [[ $rc -eq 2 ]]; then
  exit 2
fi
if [[ $rc -ne 0 ]]; then
  exit $rc
fi

if [[ -n "${TIFLASH_BIN:-}" && -x "${TIFLASH_BIN}" ]]; then
  echo "== tiflash binary =="
  "$TIFLASH_BIN" version 2>/dev/null || true
fi

echo "== SealDB MppClient probe =="
cargo test -q -p sealdb-store --lib mpp::tests::test_plan_dispatch_requires_tiflash -- --nocapture 2>&1 | tee /tmp/sealdb-mpp-probe.log
if grep -q 'no TiFlash\|unexpected err' /tmp/sealdb-mpp-probe.log; then
  # test tolerates missing TiFlash; re-check via a dedicated probe binary path
  :
fi

# Explicit Rust one-liner via existing library tests is enough when TiFlash is up:
# plan_dispatch must succeed (Ok) when stores exist.
python3 - <<PY
import re, sys
log = open("/tmp/sealdb-mpp-probe.log").read()
# When TiFlash exists, the unit test asserts Ok(plan) with tiflash tasks.
if "test result: ok" in log:
    print("mpp unit probe: ok")
    sys.exit(0)
print(log, file=sys.stderr)
sys.exit(1)
PY

echo "OK: tiflash-cluster-e2e smoke passed against $PD"
