#!/usr/bin/env bash
# Validate a git commit message against the project commit convention.
#
# Title:  <type>(<scope>): <subject>   (scope optional but preferred)
#   - at most 72 characters; up to 80 tolerated for an unusually long subject
# Body:   wrapped, at most 80 characters per line; up to 100 tolerated
#   - URLs and unbreakable tokens (a single word wider than the limit) are exempt
#   - `Co-authored-by` trailers are forbidden
#
# Install as a hook:
#   ln -sf ../../project/scripts/check_commit_msg.sh .git/hooks/commit-msg
# Check a stored message:
#   project/scripts/check_commit_msg.sh <path-to-commit-msg-file>
set -euo pipefail

msg_file=${1:?usage: check_commit_msg.sh <commit-msg-file>}
TYPES='feat|fix|docs|test|refactor|perf|style|chore|build|ci|revert'
TITLE_MAX=72
TITLE_HARD=80
BODY_MAX=80
BODY_HARD=100

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

# A line is exempt from the soft limit when it carries a URL or a token that is
# itself wider than the limit (wrapping cannot help).
is_special() {
  local line=$1 limit=$2 word
  case "$line" in
    *http://*|*https://*) return 0 ;;
  esac
  word=$(printf '%s\n' "$line" | awk '{ for (i = 1; i <= NF; i++) if (length($i) > m) m = length($i) } END { print m + 0 }')
  (( word > limit ))
}

title_len=${#title}
if (( title_len > TITLE_HARD )); then
  echo "commit-msg: title is ${title_len} chars, hard limit is ${TITLE_HARD}" >&2
  echo "  $title" >&2
  status=1
elif (( title_len > TITLE_MAX )); then
  echo "commit-msg: title is ${title_len} chars (target ${TITLE_MAX}); acceptable, keep it rare" >&2
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
  len=${#line}
  (( len <= BODY_MAX )) && continue
  is_special "$line" "$BODY_MAX" && continue
  if (( len > BODY_HARD )); then
    echo "commit-msg: body line ${lineno} is ${len} chars, hard limit is ${BODY_HARD}" >&2
    echo "  $line" >&2
    status=1
  else
    echo "commit-msg: body line ${lineno} is ${len} chars (target ${BODY_MAX}); acceptable, keep it rare" >&2
  fi
done <<< "$content"

exit "$status"
