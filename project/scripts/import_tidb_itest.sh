#!/usr/bin/env bash
# Sync TiDB tests/integrationtest/{t,r,s} into SealDB and rebuild regression manifest.
#
# Usage:
#   TIDB_ROOT=/path/to/tidb ./project/scripts/import_tidb_itest.sh
#
# Existing SealDB-authored suites are preserved under t/local/ and r/local/.
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SEALDB_ROOT=$(cd "$SCRIPT_DIR/../.." && pwd)
ITEST="$SEALDB_ROOT/tests/integrationtest"
MANIFEST_DIR="$SEALDB_ROOT/project/regression"
MANIFEST="$MANIFEST_DIR/manifest.json"

TIDB_ROOT=${TIDB_ROOT:-}
if [[ -z "$TIDB_ROOT" ]]; then
  for cand in \
    "${SEALDB_ROOT}/../pingcap/tidb" \
    "${HOME}/work/github/db/pingcap/tidb" \
    "/Users/wslu/work/github/db/pingcap/tidb"; do
    if [[ -d "${cand}/tests/integrationtest/t" ]]; then
      TIDB_ROOT=$cand
      break
    fi
  done
fi

if [[ -z "${TIDB_ROOT:-}" || ! -d "$TIDB_ROOT/tests/integrationtest/t" ]]; then
  echo "error: set TIDB_ROOT to a TiDB checkout with tests/integrationtest" >&2
  exit 1
fi

SRC="$TIDB_ROOT/tests/integrationtest"
echo "import from $SRC -> $ITEST"

mkdir -p "$ITEST/t/local" "$ITEST/r/local" "$MANIFEST_DIR"

# Preserve SealDB-local smoke suites once (flat t/*.test that are not TiDB imports).
if [[ ! -f "$ITEST/t/local/.imported_marker" ]]; then
  shopt -s nullglob
  for f in "$ITEST/t"/*.test; do
    base=$(basename "$f")
    mv "$f" "$ITEST/t/local/$base"
    if [[ -f "$ITEST/r/${base%.test}.result" ]]; then
      mv "$ITEST/r/${base%.test}.result" "$ITEST/r/local/${base%.test}.result"
    fi
  done
  shopt -u nullglob
  echo "preserved $(find "$ITEST/t/local" -name '*.test' | wc -l | tr -d ' ') local suite(s)"
  date -u +%Y-%m-%dT%H:%M:%SZ >"$ITEST/t/local/.imported_marker"
fi

# Refresh TiDB corpus (do not touch t/local or r/local).
rsync -a --delete \
  --exclude 'local/' \
  "$SRC/t/" "$ITEST/t/"
rsync -a --delete \
  --exclude 'local/' \
  "$SRC/r/" "$ITEST/r/"

# Stats JSON (zip in upstream).
if [[ -f "$SRC/s.zip" ]]; then
  rm -rf "$ITEST/s"
  unzip -q -o "$SRC/s.zip" -d "$ITEST"
  echo "extracted s/ from s.zip"
elif [[ -d "$SRC/s" ]]; then
  rsync -a --delete "$SRC/s/" "$ITEST/s/"
fi

# Copy config tomls if present.
for f in config.toml disable_new_collation.toml; do
  if [[ -f "$SRC/$f" ]]; then
    cp "$SRC/$f" "$ITEST/$f"
  fi
done

python3 - <<'PY' "$ITEST" "$MANIFEST" "$TIDB_ROOT"
import json, os, re, sys
from pathlib import Path
from datetime import datetime, timezone

itest = Path(sys.argv[1])
manifest_path = Path(sys.argv[2])
tidb_root = sys.argv[3]
t_root = itest / "t"

DEFER_RE = re.compile(
    r"(tiflash|tispark|enforce_mpp|(^|/)mpp(/|$)|casetest/mpp)",
    re.I,
)

# Wave assignment by path/name (R1..R7, D deferred, local).
def wave_for(name: str) -> str:
    n = name.lower()
    if n.startswith("local/"):
        return "local"
    if DEFER_RE.search(n):
        return "D"
    # R1 — select/types/show (+ top-level DML smoke names)
    if n in ("select", "show") or n.startswith("types/"):
        return "R1"
    if n in ("insert", "update", "delete", "replace"):
        return "R1"
    # R2 — joins / subquery / agg / window / cte
    if any(
        x in n
        for x in (
            "/join",
            "index_join",
            "subquery",
            "window",
            "/cte",
            "explain_cte",
            "naaj",
            "null_rejected",
            "topn_push",
            "agg_predicate",
            "collation_agg",
            "globalindex/aggregate",
            "union",
        )
    ) or n in ("cte", "subquery", "window_function", "index_join", "naaj"):
        return "R2"
    # R3 — access path / explain / partition / point get
    if any(
        x in n
        for x in (
            "point_get",
            "index_merge",
            "partition",
            "explain",
            "cbo",
            "physical_plan",
            "access_path",
            "ranger",
            "plan_cost",
            "clustered_index",
            "index.",
        )
    ) or n in ("index_merge", "access_path_selection", "clustered_index"):
        return "R3"
    # R4 — plan cache / prepare / collation / charset
    if any(
        x in n
        for x in (
            "plan_cache",
            "prepare",
            "collation",
            "character_set",
            "binding",
        )
    ):
        return "R4"
    # R5 — session / txn / privilege / db integration
    if (
        n.startswith("session/")
        or "db_integration" in n
        or n.startswith("privilege")
        or n.endswith("/txn")
        or "/txn" in n
    ):
        return "R5"
    # R6 — ddl / infoschema / statistics (non-tiflash)
    if n.startswith("ddl/") or n.startswith("infoschema/") or n.startswith("statistics/"):
        return "R6"
    # R7 — remaining executor/planner/load suites
    return "R7"
def status_for(wave: str, name: str) -> str:
    if wave == "local":
        return "local"
    if wave == "D":
        return "deferred"
    return "pending"

suites = []
for test in sorted(t_root.rglob("*.test")):
    rel = test.relative_to(t_root).with_suffix("")
    name = str(rel).replace("\\", "/")
    result = itest / "r" / f"{name}.result"
    wave = wave_for(name)
    suites.append({
        "name": name,
        "wave": wave,
        "status": status_for(wave, name),
        "test": f"t/{name}.test",
        "result": f"r/{name}.result",
        "has_result": result.is_file(),
        "skip_tikv": name == "local/txn",
        "owner_hint": {
            "R1": "expression,types,executor,session",
            "R2": "planner/core/rule_*,executor/{join,aggregate,window}",
            "R3": "planner/core,distsql,store/copr",
            "R4": "planner/core/plan_cache*,session,util/collate",
            "R5": "sessiontxn,domain,kv",
            "R6": "ddl,meta,infoschema",
            "R7": "planner,statistics",
            "D": "tiflash/mpp deferred",
            "local": "sealdb smoke (non-TiDB golden)",
        }.get(wave, ""),
    })

# R1 suites that exist become the first in_progress cohort (still skipped by
# default CI until SEALDB_ITEST_RUN_IN_PROGRESS=1).
r1 = [s for s in suites if s["wave"] == "R1" and s["status"] == "pending"]
# Keep all R1 pending; wave scripts promote individually.

by_status = {}
by_wave = {}
for s in suites:
    by_status[s["status"]] = by_status.get(s["status"], 0) + 1
    by_wave[s["wave"]] = by_wave.get(s["wave"], 0) + 1

doc = {
    "baseline": "tidb-v8.5.8-integrationtest",
    "tidb_root": tidb_root,
    "imported_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "policy": {
        "golden_source": "tidb",
        "sealdb_record": "forbidden unless SEALDB_ITEST_ALLOW_SEALDB_RECORD=1",
        "ci_runs": ["green", "local"],
        "dev_in_progress": "SEALDB_ITEST_RUN_IN_PROGRESS=1 or --status in_progress",
    },
    "counts": {"total": len(suites), "by_status": by_status, "by_wave": by_wave},
    "suites": suites,
}
manifest_path.parent.mkdir(parents=True, exist_ok=True)
manifest_path.write_text(json.dumps(doc, indent=2) + "\n")
print(f"wrote {manifest_path} ({len(suites)} suites)")
print("by_status:", json.dumps(by_status, sort_keys=True))
print("by_wave:", json.dumps(by_wave, sort_keys=True))
PY

echo "OK: import complete"
