#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SRC="$ROOT/project/skills"
DST="$ROOT/.opencode/skills"
MODE="symlink"

for arg in "$@"; do
  case "$arg" in
    --copy) MODE="copy" ;;
    -h|--help)
      echo "usage: $0 [--copy]"
      echo "Install project skills into .opencode/skills/ (default: symlink)"
      exit 0
      ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

if [[ ! -d "$SRC" ]]; then
  echo "error: skills dir not found: $SRC" >&2
  exit 2
fi

mkdir -p "$DST"
count=0
for f in "$SRC"/*.md; do
  [[ -f "$f" ]] || continue
  name="$(basename "$f" .md)"
  [[ "$name" == "README" ]] && continue
  mkdir -p "$DST/$name"
  if [[ "$MODE" == "copy" ]]; then
    cp "$f" "$DST/$name/SKILL.md"
  else
    ln -sfn "../../../project/skills/$name.md" "$DST/$name/SKILL.md"
  fi
  echo "installed: $DST/$name/SKILL.md ($MODE)"
  count=$((count + 1))
done

echo "done: $count skill(s) -> $DST"
