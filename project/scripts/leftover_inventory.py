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
    "CreateDatabaseStmt": "F02",
    "DropDatabaseStmt": "F02",
    "AlterTableStmt": "F18",
    "CreateIndexStmt": "F18",
    "DropIndexStmt": "F18",
    "ShowStmt": "F11",
    "UseStmt": "F11",
    "GrantStmt": "F20",
    "RevokeStmt": "F20",
    "AnalyzeTableStmt": "F21",
    "BeginStmt": "F12",
    "CommitStmt": "F12",
    "RollbackStmt": "F12",
    "ExplainStmt": "C2",
    "SetTxnStmt": "C3",
    "ExplainForStmt": "C2",
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
}

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


def is_core_owned(hint: str) -> bool:
    return bool(owners_of(hint) & CORE_OWNED)


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
        if is_core_owned(hint):
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

    lines.extend(["## PlanBuilder statement kinds still open", ""])
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
            "- Feature (F*) gaps are empty; remaining work is C2–C5 completeness gaps (C6 deferred).",
            "- C2: n-table DP / IndexMerge / cost-model-v2 stay open (EXPLAIN ANALYZE + cop-task-rows MVB done).",
            "- C3: TiKV cross-cluster and MDL stay open.",
            "- C4: completeness gaps closed (template-keyed plan cache / COM_STMT_* / user-vars / BINDING).",
            "- C5: unicode-weights / gbk-latin1 / only-full-group-by / set-global / leftover collation suites stay open.",
            "- C6 stays deferred: Cascades, TiFlash/MPP, BR/Lightning, plugins.",
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


def check_inventory(inv: dict, rust: Path) -> list[str]:
    errors = []
    features = json.loads((rust / "project/features/status.json").read_text())
    for key, item in features.get("items", {}).items():
        if item.get("status") != "core-ported":
            continue
        expected = list(item.get("gaps") or [])
        if key.startswith("C"):
            got = inv.get("completeness_gaps", {}).get(key, [])
            if expected != got:
                errors.append(
                    f"completeness gaps mismatch for {key}: status={expected} leftover={got}"
                )
            continue
        got = inv["feature_gaps"].get(key, [])
        if expected != got:
            errors.append(f"gaps mismatch for {key}: status={expected} leftover={got}")
    if not inv["open_stmts"]:
        errors.append("expected open PlanBuilder statement kinds")
    if not inv["suites"]:
        errors.append("expected integrationtest leftover suites")
    # F* feature gaps may be empty after leftover close; C* completeness gaps remain.
    if not inv["feature_gaps"] and not inv.get("completeness_gaps"):
        errors.append("expected feature or completeness gaps in the C1 denominator")
    return errors


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--tidb-root", required=True)
    ap.add_argument("--rust-root", default=".")
    ap.add_argument("--out", default="")
    ap.add_argument("--json", default="")
    ap.add_argument("--check", action="store_true")
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

    if args.check:
        errors = check_inventory(inv, rust)
        if errors:
            print("FAIL: leftover inventory check")
            for err in errors:
                print(f"- {err}")
            return 1
        print("PASS: leftover inventory check")
        print(f"denominator cases: {len(lock_cases(inv))}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
