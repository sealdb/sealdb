#!/usr/bin/env python3
"""Canonicalize Oracle/Rust JSON for stable diffs.

Removes or normalizes known non-deterministic fields:
  text, originalText, originTextPosition, offset
  plan_id, planId, column_id, columnId
  cost, actRows, runtimeStats, executionInfo
  timestamps, duration, memory, disk

Usage:
  normalize_json.py in.json [out.json]
  normalize_json.py -            # stdin -> stdout
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

DROP_KEYS = {
    "text",
    "originalText",
    "originTextPosition",
    "offset",
    "plan_id",
    "planId",
    "column_id",
    "columnId",
    "cost",
    "actRows",
    "runtimeStats",
    "executionInfo",
    "execution_info",
    "duration",
    "memory",
    "disk",
    "timestamp",
    "start_ts",
    "commit_ts",
}

FLOAT_ROUND = 6


def normalize(value):
    if isinstance(value, dict):
        out = {}
        for k, v in value.items():
            if k in DROP_KEYS:
                continue
            if k.lower() in {"id", "uid"} and isinstance(v, (int, str)):
                continue
            out[k] = normalize(v)
        return dict(sorted(out.items()))
    if isinstance(value, list):
        return [normalize(v) for v in value]
    if isinstance(value, float):
        return round(value, FLOAT_ROUND)
    if isinstance(value, str):
        return re.sub(r"\s+", " ", value).strip()
    return value


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    src = sys.argv[1]
    data = sys.stdin.read() if src == "-" else Path(src).read_text(encoding="utf-8")
    try:
        obj = json.loads(data)
    except json.JSONDecodeError as e:
        print(f"invalid JSON: {e}", file=sys.stderr)
        return 2
    text = json.dumps(normalize(obj), indent=2, ensure_ascii=False, sort_keys=True)
    if len(sys.argv) >= 3 and sys.argv[2] != "-":
        Path(sys.argv[2]).write_text(text + "\n", encoding="utf-8")
    else:
        print(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
