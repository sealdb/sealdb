#!/usr/bin/env bash
# Validate a git commit message against the project commit convention.
#
# Title:  <type>(<scope>): <subject>   (scope optional but preferred)
#   - at most 80 characters
# Body:   wrapped, at most 100 characters per line, blank line after title
#   - `Co-authored-by` trailers are forbidden
#
# Install as a hook:
#   ln -sf ../../project/scripts/check_commit_msg.sh .git/hooks/commit-msg
# Check a stored message:
#   project/scripts/check_commit_msg.sh <path-to-commit-msg-file>
set -euo pipefail

msg_file=${1:?usage: check_commit_msg.sh <commit-msg-file>}
TYPES='feat|fix|docs|test|refactor|perf|style|chore|build|ci|revert'

# Strip comment lines (`git commit` templates keep them in the file).
content=$(grep -v '^#' "$msg_file" | sed -e 's/[[:space:]]*$//')
title=$(printf '%s\n' "$content" | sed -n '/./{p;q;}')
if [[ -z "$title" ]]; then
  echo "commit-msg: empty commit message" >&2
  exit 1
fi

status=0

if [[ ! "$title" =~ ^($TYPES)(\([^\)]+\))?!?:\ .+ ]]; then
  cat >&2 <<EOF
commit-msg: title must look like "<type>(<scope>): <subject>"
  got: $title
  types: ${TYPES//|/, }
  e.g.  fix(session): ONLY_FULL_GROUP_BY validates CTE and derived tables
EOF
  status=1
fi

if (( ${#title} > 80 )); then
  echo "commit-msg: title is ${#title} chars, limit is 80" >&2
  echo "  $title" >&2
  status=1
fi

if printf '%s\n' "$content" | grep -qi '^Co-authored-by:'; then
  echo "commit-msg: 'Co-authored-by' trailers are forbidden" >&2
  status=1
fi

# Body: skip the title, keep the rest; long lines must be wrapped.
lineno=0
while IFS= read -r line; do
  lineno=$((lineno + 1))
  (( lineno == 1 )) && continue
  (( lineno == 2 )) && [[ -z "$line" ]] && continue
  (( lineno == 2 )) && {
    echo "commit-msg: second line must be blank (title/body separator)" >&2
    status=1
    continue
  }
  (( ${#line} <= 100 )) && continue
  case "$line" in
    *http://*|*https://*) continue ;;
  esac
  echo "commit-msg: body line ${lineno} is ${#line} chars, limit is 100" >&2
  echo "  $line" >&2
  status=1
done <<< "$content"

exit "$status"
