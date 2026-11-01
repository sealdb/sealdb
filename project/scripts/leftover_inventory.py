#!/usr/bin/env python3
"""Scan TiDB compute-layer entries that are not yet claimed by a SealDB feature."""

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
    "ShowStmt": "F11",
    "UseStmt": "F11",
    "AnalyzeTableStmt": "F21",
    "PrepareStmt": "C4",
    "ExecuteStmt": "C4",
    "BeginStmt": "F12",
    "CommitStmt": "F12",
    "RollbackStmt": "F12",
}


def collect_go_switch_idents(text: str) -> list[str]:
    found = []
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("case *") and "Stmt" in stripped:
            ident = stripped.split("case *", 1)[1].split(":")[0].split(",")[0].strip()
            found.append(ident)
    return found


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--tidb-root", required=True)
    ap.add_argument("--rust-root", default=".")
    ap.add_argument("--out", default="")
    args = ap.parse_args()

    tidb = Path(args.tidb_root)
    rust = Path(args.rust_root)
    features = json.loads((rust / "project/features/status.json").read_text())
    core_ported = {
        k for k, v in features.get("items", {}).items() if v.get("status") == "core-ported"
    }

    planbuilder = tidb / "pkg/planner/core/planbuilder.go"
    stmts = []
    if planbuilder.exists():
        stmts = collect_go_switch_idents(planbuilder.read_text(encoding="utf-8", errors="replace"))

    integration = tidb / "tests/integrationtest/t"
    suites = sorted(p.stem for p in integration.glob("*.test")) if integration.is_dir() else []

    leftover_stmts = [s for s in stmts if FEATURE_HINTS.get(s.split(".")[-1], "unmapped") not in core_ported]
    lines = [
        "# Leftover inventory",
        "",
        f"Baseline features core-ported: {', '.join(sorted(core_ported)) or '(none)'}",
        "",
        "## PlanBuilder statement kinds still open",
        "",
    ]
    if leftover_stmts:
        for s in leftover_stmts:
            hint = FEATURE_HINTS.get(s.split(".")[-1], "unmapped")
            lines.append(f"- `{s}` → {hint}")
    else:
        lines.append("- none")
    lines.extend(["", "## integrationtest suites", ""])
    for name in suites:
        lines.append(f"- `{name}`")
    lines.extend(
        [
            "",
            "## Completeness wave notes",
            "",
            "- C1 uses this file as the starting leftover denominator.",
            "- Do not claim pass rates until a corpus.lock.json freezes the selected leftover subset.",
            "- Cross-cluster checks (TiDB write / SealDB read and reverse) start after F03.",
        ]
    )
    text = "\n".join(lines) + "\n"
    if args.out:
        out = Path(args.out)
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(text, encoding="utf-8")
        print(f"wrote {out}")
    else:
        print(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
