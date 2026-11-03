#!/usr/bin/env python3
"""Build the C1 leftover denominator from feature gaps and TiDB leftover testdata."""

from __future__ import annotations

import argparse
import json
from pathlib import Path


FEATURE_HINTS = {
    "SelectStmt": "F01/F03/F04",
    "InsertStmt": "F03",
    "UpdateStmt": "F07",
    "DeleteStmt": "F07",
    "SetOprStmt": "F15",
    "CreateTableStmt": "F02",
    "DropTableStmt": "F02",
    "TruncateTableStmt": "F02",
    "CreateDatabaseStmt": "F02",
    "DropDatabaseStmt": "F02",
    "AlterDatabaseStmt": "F02",
    "DoStmt": "F01",
    "SetPwdStmt": "F20",
    "FlushStmt": "F20",
    "ShutdownStmt": "F22",
    "KillStmt": "F22",
    "BinlogStmt": "F22",
    "ChangeStmt": "F22",
    "AlterInstanceStmt": "F22",
    "AlterTableStmt": "F18",
    "CreateIndexStmt": "F18",
    "DropIndexStmt": "F18",
    "RenameTableStmt": "F18",
    "AdminStmt": "F18",
    "RepairTableStmt": "F18",
    "ShowStmt": "F11",
    "UseStmt": "F11",
    "GrantStmt": "F20",
    "RevokeStmt": "F20",
    "DropUserStmt": "F20",
    "RenameUserStmt": "F20",
    "GrantRoleStmt": "F20",
    "RevokeRoleStmt": "F20",
    "AnalyzeTableStmt": "F21",
    "OptimizeTableStmt": "F21",
    "LockTablesStmt": "F12",
    "CleanupTableLockStmt": "F12",
    "BeginStmt": "F12",
    "CommitStmt": "F12",
    "RollbackStmt": "F12",
    "ExplainStmt": "C2",
    "ExplainForStmt": "C2",
    "SetTxnStmt": "C3",
    "PrepareStmt": "C4",
    "ExecuteStmt": "C4",
    "DeallocateStmt": "C4",
    "CreateBindingStmt": "C4",
    "DropBindingStmt": "C4",
    "SetBindingStmt": "C4",
    "SetStmt": "C5",
    "SetConfigStmt": "C5",
    "BRIEStmt": "C6",
    "ImportIntoStmt": "C6",
    "LoadDataStmt": "C6",
    "CalibrateResourceStmt": "C6",
    "TraceStmt": "C6",
    "PlanReplayerStmt": "C6",
    "LoadStatsStmt": "C6",
    "LockStatsStmt": "C6",
    "UnlockStatsStmt": "C6",
    "FlashBackTableStmt": "C6",
    "FlashBackDatabaseStmt": "C6",
    "FlashBackToTimestampStmt": "C6",
    "RecoverTableStmt": "C6",
    "CreateResourceGroupStmt": "C6",
    "SetResourceGroupStmt": "C6",
    "CreateViewStmt": "C6",
    "CreateSequenceStmt": "C6",
    "DropSequenceStmt": "C6",
    "AlterSequenceStmt": "C6",
    "DropPlacementPolicyStmt": "C6",
    "CompactTableStmt": "C6",
    "DistributeTableStmt": "C6",
    "SplitRegionStmt": "C6",
    "RecommendIndexStmt": "C6",
    "AddQueryWatchStmt": "C6",
    "DropQueryWatchStmt": "C6",
}

# C6 production-parity slices already landed: leave the open leftover surface.
C6_CLOSED_STMTS: frozenset[str] = frozenset({"CreateViewStmt"})

# Frozen allowlist: PlanBuilder kinds intentionally left `unmapped` (not C6 and
# not owned by a closed F*/C2–C5 wave). C1 verifies open unmapped == this set.
# Empty when every non-C6 PlanBuilder kind is owned by a closed F*/C2–C5 wave.
UNMAPPED_ALLOWLIST: frozenset[str] = frozenset()

SUITE_HINTS = {
    "access_path_selection": "C2",
    "agg_predicate_pushdown": "C2",
    "black_list": "C6",
    "clustered_index": "C2",
    "collation_agg_func": "C5",
    "collation_check_use_collation": "C5",
    "collation_misc": "C5",
    "collation_pointget": "C5",
    "common_collation": "C5",
    "cte": "F17",
    "db_integration": "C3",
    "explain": "C2",
    "explain-non-select-stmt": "C2",
    "explain_complex": "C2",
    "explain_complex_stats": "C2",
    "explain_cte": "C2",
    "explain_easy": "C2",
    "explain_easy_stats": "C2",
    "explain_foreign_key": "C2",
    "explain_generate_column_substitute": "C2",
    "explain_indexmerge_stats": "C2",
    "explain_join_stats": "C2",
    "explain_shard_index": "C2",
    "explain_stats": "C2",
    "explain_union_scan": "C2",
    "generated_columns": "C2",
    "imdbload": "C2",
    "index_join": "C2",
    "index_merge": "C2",
    "naaj": "C2",
    "new_character_set": "C5",
    "new_character_set_builtin": "C5",
    "new_character_set_invalid": "C5",
    "null_rejected": "C2",
    "partition": "F19",
    "select": "F04",
    "show": "F11",
    "subquery": "F14",
    "topn_push_down": "F05",
    "topn_pushdown": "F05",
    "tpch": "C2",
    "window_function": "F16",
}

# Suites whose MVB is covered by a named feature_gate.sh step (not full suite port).
SUITE_GATE_COVERAGE = {
    "C2": ("execute-explain", ("explain", "index_merge", "index_join")),
    "C5": (
        "execute-leftover-collation-suites",
        ("new_character_set", "collation_", "common_collation"),
    ),
}

CORE_OWNED = {
    "F01",
    "F02",
    "F03",
    "F04",
    "F05",
    "F06",
    "F07",
    "F08",
    "F09",
    "F10",
    "F11",
    "F12",
    "F13",
    "F14",
    "F15",
    "F16",
    "F17",
    "F18",
    "F19",
    "F20",
    "F21",
    "F22",
}

# Completeness waves with gaps []; their stmts leave the open leftover surface.
COMPLETENESS_CLOSED = {"C2", "C3", "C4", "C5"}


def collect_go_switch_idents(text: str) -> list[str]:
    found = []
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("case *") and "Stmt" in stripped:
            ident = stripped.split("case *", 1)[1].split(":")[0].split(",")[0].strip()
            found.append(ident)
    return found


def stmt_name(ident: str) -> str:
    return ident.split(".")[-1]


def owners_of(hint: str) -> set[str]:
    return {part.strip() for part in hint.split("/") if part.strip()}


def is_closed_owned(hint: str) -> bool:
    owners = owners_of(hint)
    return bool(owners & CORE_OWNED) or bool(owners & COMPLETENESS_CLOSED)


def collect_inventory(tidb: Path, rust: Path) -> dict:
    features = json.loads((rust / "project/features/status.json").read_text())
    items = features.get("items", {})
    core_ported = sorted(k for k, v in items.items() if v.get("status") == "core-ported")
    feature_gaps = {
        k: list(v.get("gaps") or [])
        for k, v in items.items()
        if v.get("status") == "core-ported" and v.get("gaps") and not k.startswith("C")
    }
    completeness_gaps = {
        k: list(v.get("gaps") or [])
        for k, v in items.items()
        if v.get("status") == "core-ported" and v.get("gaps") and k.startswith("C")
    }

    planbuilder = tidb / "pkg/planner/core/planbuilder.go"
    stmts = []
    if planbuilder.exists():
        stmts = collect_go_switch_idents(planbuilder.read_text(encoding="utf-8", errors="replace"))

    open_stmts = []
    seen = set()
    for ident in stmts:
        name = stmt_name(ident)
        if name in seen:
            continue
        seen.add(name)
        hint = FEATURE_HINTS.get(name, "unmapped")
        if is_closed_owned(hint) or name in C6_CLOSED_STMTS:
            continue
        open_stmts.append({"stmt": ident, "owner": hint})

    integration = tidb / "tests/integrationtest/t"
    suites = []
    if integration.is_dir():
        for name in sorted(p.stem for p in integration.glob("*.test")):
            suites.append({"name": name, "owner": SUITE_HINTS.get(name, "unmapped")})

    return {
        "frozen": "2026-11-01",
        "baseline": features.get("baseline", "v8.5.8"),
        "core_ported": core_ported,
        "feature_gaps": feature_gaps,
        "completeness_gaps": completeness_gaps,
        "unmapped_allowlist": sorted(UNMAPPED_ALLOWLIST),
        "open_stmts": open_stmts,
        "suites": suites,
    }


def render_markdown(inv: dict) -> str:
    lines = [
        "# Leftover inventory",
        "",
        f"Baseline features core-ported: {', '.join(inv['core_ported']) or '(none)'}",
        "",
        "C1 freezes this file plus `project/features/leftover.json` as the completeness denominator.",
        "Do not claim leftover pass rates until a later wave marks a subset `ported`.",
        "",
        "## Feature gaps still open",
        "",
    ]
    if inv["feature_gaps"]:
        for feature, gaps in inv["feature_gaps"].items():
            lines.append(f"### {feature}")
            lines.append("")
            for gap in gaps:
                lines.append(f"- `{gap}`")
            lines.append("")
    else:
        lines.extend(["- none", ""])

    lines.extend(["## Completeness gaps still open", ""])
    if inv.get("completeness_gaps"):
        for feature, gaps in inv["completeness_gaps"].items():
            lines.append(f"### {feature}")
            lines.append("")
            for gap in gaps:
                lines.append(f"- `{gap}`")
            lines.append("")
    else:
        lines.extend(["- none", ""])

    lines.extend(
        [
            "## Frozen unmapped PlanBuilder allowlist",
            "",
            "These kinds stay `unmapped` on purpose (not claimed by F*/C2–C5, not C6).",
            "C1 `--check` requires open `unmapped` stmts to equal this set.",
            "",
        ]
    )
    allow_names = inv["unmapped_allowlist"] if "unmapped_allowlist" in inv else sorted(
        UNMAPPED_ALLOWLIST
    )
    if allow_names:
        for name in allow_names:
            lines.append(f"- `{name}`")
    else:
        lines.append("- none")
    lines.extend(["", "## PlanBuilder statement kinds still open", ""])
    if inv["open_stmts"]:
        for item in inv["open_stmts"]:
            lines.append(f"- `{item['stmt']}` → {item['owner']}")
    else:
        lines.append("- none")
    lines.extend(["", "## integrationtest leftover suites", ""])
    for item in inv["suites"]:
        lines.append(f"- `{item['name']}` → {item['owner']}")
    lines.extend(
        [
            "",
            "## Completeness wave notes",
            "",
            "- C1 is the frozen leftover denominator, not a pass-rate claim.",
            "- Feature (F*) gaps are empty; C1–C5 completeness gaps closed.",
            "- C2: completeness gaps closed (Cost Model v2 MVB + IndexMerge OR-union).",
            "- C3: completeness gaps closed (MDL + schema-lease 8027 + SERIALIZABLE→RR + SAVEPOINT F12 + same-cluster TiKV cross-session when SEALDB_TIKV=1).",
            "- C4: completeness gaps closed (template-keyed plan cache / COM_STMT_* / user-vars / BINDING).",
            "- C5: completeness gaps closed (unicode-weights + gbk-latin1 + only-full-group-by + set-global + leftover collation suites MVB; new_character_set* exercised by test_leftover_collation_suites).",
            "- Leftover suite execution: suites are owner-classified; C2 `explain*` / `index_merge` MVB via `execute-explain*`; C5 collation / `new_character_set*` via `execute-leftover-collation-suites`. Full integrationtest port is not claimed.",
            "- C6 is production parity: package/file align TiDB compute; order A/B → C (compute + full PD/TiKV client) → D (TiFlash; deferred not dropped). Closed C6 stmts: C6_CLOSED_STMTS. Open stmts = B backlog.",
            "",
        ]
    )
    return "\n".join(lines)


def lock_cases(inv: dict) -> list[str]:
    cases = []
    for feature, gaps in inv["feature_gaps"].items():
        for gap in gaps:
            cases.append(f"{feature}:{gap}")
    for item in inv["open_stmts"]:
        cases.append(f"stmt:{item['stmt']}")
    for item in inv["suites"]:
        cases.append(f"suite:{item['name']}")
    return cases


def check_unmapped_allowlist(inv: dict) -> list[str]:
    errors = []
    open_unmapped = {
        stmt_name(item["stmt"])
        for item in inv["open_stmts"]
        if item.get("owner") == "unmapped"
    }
    # Empty list is a valid terminal allowlist (all non-C6 stmts owned).
    if "unmapped_allowlist" in inv:
        allow = set(inv["unmapped_allowlist"])
    else:
        allow = set(UNMAPPED_ALLOWLIST)
    unexpected = sorted(open_unmapped - allow)
    missing = sorted(allow - open_unmapped)
    if unexpected:
        errors.append(
            "unmapped stmts not on frozen allowlist: " + ", ".join(unexpected)
        )
    if missing:
        errors.append(
            "allowlist entries missing from open unmapped stmts: " + ", ".join(missing)
        )
    return errors


def check_stale_gap_labels(inv: dict, rust: Path) -> list[str]:
    errors = []
    features = json.loads((rust / "project/features/status.json").read_text())
    for key, item in features.get("items", {}).items():
        if item.get("status") != "core-ported":
            continue
        gaps = list(item.get("gaps") or [])
        if key.startswith("F") and gaps:
            errors.append(f"F* feature {key} still has non-empty gaps: {gaps}")
        if key.startswith("C") and key != "C6" and gaps:
            errors.append(
                f"completeness feature {key} still has non-empty gaps: {gaps}"
            )
        expected = gaps
        if key.startswith("C"):
            got = inv.get("completeness_gaps", {}).get(key, [])
            if expected != got:
                errors.append(
                    f"completeness gaps mismatch for {key}: status={expected} leftover={got}"
                )
        else:
            got = inv["feature_gaps"].get(key, [])
            if expected != got:
                errors.append(
                    f"gaps mismatch for {key}: status={expected} leftover={got}"
                )
    return errors


def check_suite_smoke(inv: dict, rust: Path) -> list[str]:
    errors = []
    for suite in inv["suites"]:
        owner = (suite.get("owner") or "").strip()
        if not owner:
            errors.append(f"suite {suite['name']} has empty owner")
        elif owner == "unmapped":
            errors.append(f"suite {suite['name']} is unmapped (must be classified)")

    gate = (rust / "project/scripts/feature_gate.sh").read_text(encoding="utf-8")
    covered_owners = []
    for owner, (gate_step, prefixes) in SUITE_GATE_COVERAGE.items():
        owned = [s["name"] for s in inv["suites"] if s.get("owner") == owner]
        if not owned:
            continue
        if gate_step not in gate:
            errors.append(
                f"feature_gate.sh missing {gate_step} for {owner}-owned leftover suites"
            )
            continue
        if not any(
            any(name.startswith(prefix) or name == prefix for prefix in prefixes)
            for name in owned
        ):
            errors.append(
                f"no {owner}-owned leftover suite matches coverage prefixes {prefixes}"
            )
            continue
        covered_owners.append(owner)

    if "C2" not in covered_owners and "C5" not in covered_owners:
        errors.append(
            "expected at least one C2/C5-owned leftover suite covered by a feature_gate test"
        )
    return errors


def check_inventory(inv: dict, rust: Path) -> list[str]:
    errors = []
    errors.extend(check_stale_gap_labels(inv, rust))
    errors.extend(check_unmapped_allowlist(inv))
    if not inv["open_stmts"]:
        errors.append("expected open PlanBuilder statement kinds (C6 + allowlist)")
    if not inv["suites"]:
        errors.append("expected integrationtest leftover suites")
    # Terminal honest state: F* and C1–C5 gaps empty; open_stmts/suites remain.
    if inv["feature_gaps"] or inv.get("completeness_gaps"):
        errors.append(
            "expected empty feature_gaps and completeness_gaps after C1–C5 close "
            f"(feature_gaps={inv['feature_gaps']}, completeness_gaps={inv.get('completeness_gaps')})"
        )
    return errors


def write_corpus_lock(inv: dict, rust: Path) -> Path:
    path = rust / "project/reports/features/C1/corpus.lock.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    cases = lock_cases(inv)
    payload = {
        "feature": "C1",
        "frozen": inv.get("frozen", "2026-11-01"),
        "source": [
            "project/features/status.json gaps[]",
            "TiDB pkg/planner/core/planbuilder.go Build",
            "TiDB tests/integrationtest/t/*.test",
            "FEATURE_HINTS / UNMAPPED_ALLOWLIST / SUITE_HINTS",
        ],
        "cases": cases,
        "counts": {
            "feature_gaps": sum(len(v) for v in inv["feature_gaps"].values()),
            "completeness_gaps": sum(
                len(v) for v in (inv.get("completeness_gaps") or {}).values()
            ),
            "open_stmts": len(inv["open_stmts"]),
            "suites": len(inv["suites"]),
            "unmapped_allowlist": len(inv.get("unmapped_allowlist") or []),
            "total": len(cases),
        },
    }
    path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    return path


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--tidb-root", required=True)
    ap.add_argument("--rust-root", default=".")
    ap.add_argument("--out", default="")
    ap.add_argument("--json", default="")
    ap.add_argument("--check", action="store_true")
    ap.add_argument(
        "--suite-smoke",
        action="store_true",
        help="assert leftover suites are classified and C2/C5 MVB gate coverage exists",
    )
    ap.add_argument(
        "--write-corpus-lock",
        action="store_true",
        help="refresh project/reports/features/C1/corpus.lock.json from inventory",
    )
    args = ap.parse_args()

    rust = Path(args.rust_root)
    inv = collect_inventory(Path(args.tidb_root), rust)
    text = render_markdown(inv)

    if args.out:
        out = Path(args.out)
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(text, encoding="utf-8")
        print(f"wrote {out}")
    else:
        print(text, end="")

    if args.json:
        path = Path(args.json)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(inv, indent=2) + "\n", encoding="utf-8")
        print(f"wrote {path}")

    if args.write_corpus_lock:
        lock_path = write_corpus_lock(inv, rust)
        print(f"wrote {lock_path}")

    if args.suite_smoke:
        errors = check_suite_smoke(inv, rust)
        if errors:
            print("FAIL: leftover suite smoke")
            for err in errors:
                print(f"- {err}")
            return 1
        print("PASS: leftover suite smoke")
        print(
            "classified suites: "
            f"{len(inv['suites'])}; C2/C5 gate coverage via "
            + ", ".join(
                f"{owner}:{step}" for owner, (step, _) in SUITE_GATE_COVERAGE.items()
            )
        )
        return 0

    if args.check:
        errors = check_inventory(inv, rust)
        if errors:
            print("FAIL: leftover inventory check")
            for err in errors:
                print(f"- {err}")
            return 1
        print("PASS: leftover inventory check")
        print(f"denominator cases: {len(lock_cases(inv))}")
        print(
            f"open_stmts={len(inv['open_stmts'])} "
            f"(unmapped_allowlist={len(inv.get('unmapped_allowlist') or [])}) "
            f"suites={len(inv['suites'])}"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
