#!/usr/bin/env python3
"""Parity checker driven by project/parity/status.json.

The manifest is the only source of truth for port status. Source files keep no
upstream provenance markers.

Manifest schema:
  {
    "baseline": "v8.5.8",
    "files": {
      "pkg/parser/parser.go": {
        "status": "ported|partial|reimplemented|stub|omitted|not-started",
        "rust": "pkg/parser/parser.rs",
        "phase": 2,
        "notes": "..."
      }
    }
  }

Exit codes:
  0  no blocking issues
  1  blocking issues (missing dirs, required package incomplete, or --strict
     failures such as unmapped .rs / extra files)
  2  usage or IO error
"""

from __future__ import annotations

import argparse
import json
import os
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
ACTIVE_STATUS = {"ported", "partial", "stub", "reimplemented"}
DONE_STATUS = {"ported", "partial", "reimplemented", "omitted"}

ALLOWED_RS_NAMES = {"mod.rs", "lib.rs"}
ALLOWED_RS_SUFFIXES = ("_test.rs",)
ALLOWED_RS_PATH_PARTS = ("sqlparser_bridge",)
IGNORED_RS_DIRS = {"target", ".git"}


@dataclass
class FileRow:
    go_path: str
    rs_path: str | None
    status: str
    phase: int | None = None
    notes: str | None = None


@dataclass
class Report:
    tidb_root: str
    rust_root: str
    manifest: str
    baseline: str | None = None
    required: list[str] = field(default_factory=list)
    go_files: int = 0
    rs_files: int = 0
    manifest_entries: int = 0
    status_counts: dict[str, int] = field(default_factory=dict)
    missing_dirs: list[str] = field(default_factory=list)
    missing_rs: list[str] = field(default_factory=list)
    invalid_entries: list[str] = field(default_factory=list)
    unmapped_rs: list[str] = field(default_factory=list)
    extra_files: list[str] = field(default_factory=list)
    incomplete_required: list[str] = field(default_factory=list)
    packages: dict[str, dict] = field(default_factory=dict)
    rows: list[FileRow] = field(default_factory=list)

    @property
    def blocking(self) -> bool:
        return bool(
            self.missing_dirs
            or self.missing_rs
            or self.invalid_entries
            or self.incomplete_required
        )


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


def load_manifest(path: Path) -> dict:
    if not path.exists():
        return {"baseline": None, "files": {}}
    with path.open("r", encoding="utf-8") as f:
        data = json.load(f)
    if not isinstance(data, dict):
        raise ValueError("manifest root must be an object")
    files = data.get("files", {})
    if not isinstance(files, dict):
        raise ValueError("manifest.files must be an object")
    return data


def main() -> int:
    ap = argparse.ArgumentParser(description="Parity checker (manifest-driven)")
    ap.add_argument("--tidb-root", required=True)
    ap.add_argument("--rust-root", default=".")
    ap.add_argument("--manifest", default="")
    ap.add_argument(
        "--require",
        default="",
        help="Comma-separated package dirs that must be complete",
    )
    ap.add_argument("--strict", action="store_true", help="Fail on invalid/unmapped/extra files")
    ap.add_argument("--include-extra", action="store_true", help="Report extra .rs files")
    ap.add_argument("--json", metavar="PATH", help="Write JSON report ('-' = stdout)")
    ap.add_argument("--limit", type=int, default=50)
    args = ap.parse_args()

    tidb_root = Path(args.tidb_root).resolve()
    rust_root = Path(args.rust_root).resolve()
    manifest_path = (
        Path(args.manifest).resolve()
        if args.manifest
        else (rust_root / "project/parity/status.json")
    )

    if not (tidb_root / "pkg").is_dir():
        print(f"error: {tidb_root} does not look like the reference repo", file=sys.stderr)
        return 2
    if not rust_root.exists():
        print(f"error: rust root {rust_root} does not exist", file=sys.stderr)
        return 2

    try:
        manifest = load_manifest(manifest_path)
    except (OSError, ValueError, json.JSONDecodeError) as e:
        print(f"error: invalid manifest {manifest_path}: {e}", file=sys.stderr)
        return 2

    entries: dict[str, dict] = manifest.get("files", {})
    required = [s.strip().strip("/") for s in args.require.split(",") if s.strip()]
    subdirs = ["pkg", "cmd"]

    report = Report(
        str(tidb_root),
        str(rust_root),
        str(manifest_path),
        baseline=manifest.get("baseline"),
        required=required,
    )

    go_map = {rel: p for rel, p in iter_go_files(tidb_root, subdirs)}
    rs_map = {rel: p for rel, p in iter_rs_files(rust_root, subdirs)}
    report.go_files = len(go_map)
    report.rs_files = len(rs_map)
    report.manifest_entries = len(entries)

    counts: Counter[str] = Counter()
    pkg_stats: dict[str, dict] = defaultdict(
        lambda: {"go": 0, "rs": 0, "missing": 0, "status": Counter()}
    )
    mapped_rs: set[str] = set()

    for go_rel in sorted(go_map):
        pkg = package_of(go_rel)
        pkg_stats[pkg]["go"] += 1
        entry = entries.get(go_rel)
        status = (entry or {}).get("status", "not-started")
        phase = (entry or {}).get("phase")
        notes = (entry or {}).get("notes")

        if status not in VALID_STATUS:
            report.invalid_entries.append(f"{go_rel}: unknown status {status!r}")
            status = "invalid"

        rs_rel = (entry or {}).get("rust") or go_to_rs(go_rel)
        rs_exists = (rust_root / rs_rel).exists()
        if rs_exists:
            mapped_rs.add(rs_rel)
            pkg_stats[pkg]["rs"] += 1
        else:
            pkg_stats[pkg]["missing"] += 1

        if status in ACTIVE_STATUS and not rs_exists:
            report.missing_rs.append(f"{go_rel} -> {rs_rel}")
        if status == "omitted" and rs_exists:
            report.invalid_entries.append(f"{go_rel}: omitted but {rs_rel} exists")
        if entry is None and rs_exists:
            report.invalid_entries.append(f"{rs_rel}: exists but is not in manifest")

        if required and go_rel.startswith(tuple(r + "/" for r in required)):
            if status not in DONE_STATUS:
                report.incomplete_required.append(f"{go_rel} [{status}]")

        counts[status] += 1
        pkg_stats[pkg]["status"][status] += 1
        report.rows.append(
            FileRow(go_rel, rs_rel if rs_exists else None, status, phase, notes)
        )

    for req in required:
        if not (rust_root / req).is_dir():
            report.missing_dirs.append(req)

    if args.include_extra or args.strict:
        for rs_rel in sorted(rs_map):
            if rs_rel in mapped_rs or is_allowed_extra(rs_rel):
                continue
            report.extra_files.append(rs_rel)

    report.status_counts = dict(counts)
    report.packages = {
        k: {"go": v["go"], "rs": v["rs"], "missing": v["missing"], "status": dict(v["status"])}
        for k, v in sorted(pkg_stats.items())
    }

    print("== Parity Report ==")
    print(f"TiDB root : {tidb_root}")
    print(f"Rust root : {rust_root}")
    print(f"Manifest  : {manifest_path}")
    print(f"Baseline  : {report.baseline or 'unset'}")
    print(f"Go files  : {report.go_files}")
    print(f"Rust files: {report.rs_files}")
    print(f"Entries   : {report.manifest_entries}")
    print("Status    : " + ", ".join(f"{k}={v}" for k, v in sorted(counts.items())))

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
    show("Missing Rust files (active status)", report.missing_rs)
    show("Invalid manifest entries", report.invalid_entries)
    show("Incomplete required files", report.incomplete_required)
    show("Extra Rust files", report.extra_files)

    if not counts:
        print("\nNo Go files found. Check --tidb-root.")
    elif counts.get("ported", 0) == 0 and counts.get("partial", 0) == 0:
        print("\nNo ported/partial files yet. This is expected before Phase 1.")

    payload = asdict(report)
    payload["blocking"] = report.blocking or bool(
        args.strict and (report.invalid_entries or report.extra_files)
    )
    if args.json:
        text = json.dumps(payload, indent=2, ensure_ascii=False)
        if args.json == "-":
            print(text)
        else:
            out = Path(args.json)
            out.parent.mkdir(parents=True, exist_ok=True)
            out.write_text(text + "\n", encoding="utf-8")
            print(f"\nJSON report: {out}")

    if report.blocking:
        return 1
    if args.strict and (report.invalid_entries or report.extra_files):
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
