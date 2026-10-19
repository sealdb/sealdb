#!/usr/bin/env python3
"""TiDB -> Rust parity checker.

Scans the TiDB reference tree (pkg/ and cmd/) and the Rust tree, maps
`foo.go` -> `foo.rs`, reads `PORT_STATUS` headers, and reports:

  - missing directories
  - Go files without a Rust counterpart (not-started)
  - Rust files without a valid status header (invalid)
  - explicit status counts (ported/partial/stub/reimplemented/omitted)
  - extra Rust files outside the allow-list

Exit codes:
  0  no blocking issues
  1  blocking issues (invalid headers with --strict-headers, or missing
     directories/files inside --require)
  2  usage / IO error

This script intentionally does not modify files.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
from collections import Counter, defaultdict
from dataclasses import dataclass, field, asdict
from pathlib import Path

VALID_STATUS = {
    "ported",
    "partial",
    "stub",
    "reimplemented",
    "omitted",
    "not-started",
}

STATUS_RE = re.compile(
    r"PORT_STATUS:\s*(ported|partial|stub|reimplemented|omitted|not-started)\b"
)
SOURCE_RE = re.compile(r"PORT_SOURCE:\s*(.+?)\s*$", re.MULTILINE)

ALLOWED_RS_NAMES = {"mod.rs", "lib.rs"}
ALLOWED_RS_SUFFIXES = ("_test.rs",)
ALLOWED_RS_PATH_PARTS = ("sqlparser_bridge",)
IGNORED_RS_DIRS = {"target", ".git"}


@dataclass
class FileRow:
    go_path: str
    rs_path: str | None
    status: str
    source: str | None = None
    notes: str | None = None


@dataclass
class Report:
    tidb_root: str
    rust_root: str
    required: list[str] = field(default_factory=list)
    go_files: int = 0
    rs_files: int = 0
    status_counts: dict[str, int] = field(default_factory=dict)
    missing_dirs: list[str] = field(default_factory=list)
    invalid_headers: list[str] = field(default_factory=list)
    extra_files: list[str] = field(default_factory=list)
    missing_in_required: list[str] = field(default_factory=list)
    packages: dict[str, dict] = field(default_factory=dict)
    rows: list[FileRow] = field(default_factory=list)

    @property
    def blocking(self) -> bool:
        return bool(self.missing_dirs or self.missing_in_required)


def iter_go_files(root: Path, subdirs: list[str]):
    for sub in subdirs:
        base = root / sub
        if not base.exists():
            continue
        for p in base.rglob("*.go"):
            if p.name.endswith("_test.go"):
                continue
            rel = p.relative_to(root).as_posix()
            if "/testdata/" in rel or rel.endswith("/test_driver.go"):
                continue
            yield rel, p


def iter_rs_files(root: Path, subdirs: list[str]):
    for sub in subdirs:
        base = root / sub
        if not base.exists():
            continue
        for p in base.rglob("*.rs"):
            rel = p.relative_to(root).as_posix()
            if any(part in IGNORED_RS_DIRS for part in p.parts):
                continue
            yield rel, p


def go_to_rs(go_rel: str) -> str:
    return go_rel[:-3] + ".rs" if go_rel.endswith(".go") else go_rel


def read_header(path: Path) -> tuple[str | None, str | None, str | None]:
    try:
        with path.open("r", encoding="utf-8", errors="replace") as f:
            head = "".join(f.readline() for _ in range(80))
    except OSError:
        return None, None, None

    m = STATUS_RE.search(head)
    status = m.group(1) if m else None
    sm = SOURCE_RE.search(head)
    source = sm.group(1).strip() if sm else None
    nm = re.search(r"PARITY_NOTES:\s*(.+)", head)
    notes = nm.group(1).strip() if nm else None
    return status, source, notes


def is_allowed_extra(rs_rel: str) -> bool:
    name = os.path.basename(rs_rel)
    if name in ALLOWED_RS_NAMES:
        return True
    if name.endswith(ALLOWED_RS_SUFFIXES):
        return True
    return any(part in rs_rel for part in ALLOWED_RS_PATH_PARTS)


def package_of(rel: str) -> str:
    parts = rel.split("/")
    if len(parts) >= 2 and parts[0] in ("pkg", "cmd"):
        return "/".join(parts[:2])
    return parts[0] if parts else ""


def main() -> int:
    ap = argparse.ArgumentParser(description="TiDB -> Rust parity checker")
    ap.add_argument("--tidb-root", required=True, help="TiDB reference repo root")
    ap.add_argument("--rust-root", default=".", help="Rust repo root (default: .)")
    ap.add_argument(
        "--require",
        default="",
        help="Comma-separated package dirs that must exist (e.g. pkg/parser,pkg/types)",
    )
    ap.add_argument(
        "--strict-headers",
        action="store_true",
        help="Fail if any existing .rs file lacks a valid PORT_STATUS",
    )
    ap.add_argument(
        "--include-extra",
        action="store_true",
        help="Report extra .rs files not matching a Go file",
    )
    ap.add_argument("--json", metavar="PATH", help="Write JSON report to PATH ('-' = stdout)")
    ap.add_argument("--limit", type=int, default=50, help="Max detail lines per section")
    args = ap.parse_args()

    tidb_root = Path(args.tidb_root).resolve()
    rust_root = Path(args.rust_root).resolve()
    if not (tidb_root / "pkg").is_dir():
        print(f"error: {tidb_root} does not look like a TiDB repo (pkg/ missing)", file=sys.stderr)
        return 2
    if not rust_root.exists():
        print(f"error: rust root {rust_root} does not exist", file=sys.stderr)
        return 2

    required = [s.strip().strip("/") for s in args.require.split(",") if s.strip()]
    subdirs = ["pkg", "cmd"]

    report = Report(str(tidb_root), str(rust_root), required=required)

    go_map: dict[str, Path] = {}
    for rel, p in iter_go_files(tidb_root, subdirs):
        go_map[rel] = p

    rs_map: dict[str, Path] = {}
    for rel, p in iter_rs_files(rust_root, subdirs):
        rs_map[rel] = p

    report.go_files = len(go_map)
    report.rs_files = len(rs_map)

    counts = Counter()
    pkg_stats: dict[str, dict] = defaultdict(lambda: {"go": 0, "rs": 0, "missing": 0, "status": Counter()})

    for go_rel in sorted(go_map):
        rs_rel = go_to_rs(go_rel)
        pkg = package_of(go_rel)
        pkg_stats[pkg]["go"] += 1
        rs_path = rust_root / rs_rel
        if not rs_path.exists():
            counts["not-started"] += 1
            pkg_stats[pkg]["missing"] += 1
            report.rows.append(FileRow(go_rel, None, "not-started"))
            if required and any(go_rel.startswith(r + "/") or go_rel == r for r in required):
                report.missing_in_required.append(go_rel)
            continue

        pkg_stats[pkg]["rs"] += 1
        status, source, notes = read_header(rs_path)
        if status is None:
            counts["invalid"] += 1
            report.invalid_headers.append(rs_rel)
            report.rows.append(FileRow(go_rel, rs_rel, "invalid"))
            if required and any(go_rel.startswith(r + "/") for r in required):
                report.missing_in_required.append(go_rel)
            continue

        counts[status] += 1
        pkg_stats[pkg]["status"][status] += 1
        report.rows.append(FileRow(go_rel, rs_rel, status, source, notes))

    for req in required:
        if not (rust_root / req).is_dir():
            report.missing_dirs.append(req)

    if args.include_extra:
        for rs_rel in sorted(rs_map):
            if is_allowed_extra(rs_rel):
                continue
            if go_to_rs(rs_rel) not in go_map:
                report.extra_files.append(rs_rel)

    report.status_counts = dict(counts)
    report.packages = {
        k: {"go": v["go"], "rs": v["rs"], "missing": v["missing"], "status": dict(v["status"])}
        for k, v in sorted(pkg_stats.items())
    }

    print("== Parity Report ==")
    print(f"TiDB root : {tidb_root}")
    print(f"Rust root : {rust_root}")
    print(f"Go files  : {report.go_files}")
    print(f"Rust files: {report.rs_files}")
    print("Status    : " + ", ".join(f"{k}={v}" for k, v in sorted(counts.items())))
    print(
        f"Issues    : invalid={len(report.invalid_headers)}, "
        f"missing-dirs={len(report.missing_dirs)}, extra={len(report.extra_files)}"
    )

    if required:
        print("\n== Required packages ==")
        for req in required:
            st = report.packages.get(req, {"go": 0, "rs": 0, "missing": 0})
            exists = (rust_root / req).is_dir()
            print(
                f"{req:32s} dir={'OK' if exists else 'MISSING':7s} "
                f"go={st['go']:4d} rs={st['rs']:4d} missing={st['missing']:4d}"
            )

    def show(title: str, items: list[str]):
        if not items:
            return
        print(f"\n== {title} ({len(items)}) ==")
        for x in items[: args.limit]:
            print(f"  {x}")
        if len(items) > args.limit:
            print(f"  ... {len(items) - args.limit} more")

    show("Missing directories", report.missing_dirs)
    show("Invalid headers (no PORT_STATUS)", report.invalid_headers)
    show("Missing files in required packages", report.missing_in_required)
    show("Extra Rust files", report.extra_files)

    if not counts:
        print("\nNo Rust files found. Create the Phase 0 skeleton first.")
    elif counts.get("ported", 0) == 0 and counts.get("partial", 0) == 0:
        print("\nNo ported/partial files yet. This is expected before Phase 1.")

    payload = asdict(report)
    payload["blocking"] = report.blocking or bool(report.invalid_headers and args.strict_headers)
    if args.json:
        text = json.dumps(payload, indent=2, ensure_ascii=False)
        if args.json == "-":
            print(text)
        else:
            out = Path(args.json)
            out.parent.mkdir(parents=True, exist_ok=True)
            out.write_text(text + "\n", encoding="utf-8")
            print(f"\nJSON report: {out}")

    if report.missing_dirs:
        return 1
    if report.missing_in_required:
        return 1
    if args.strict_headers and report.invalid_headers:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
