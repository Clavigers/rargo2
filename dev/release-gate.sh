#!/bin/bash
set -euo pipefail
commit=${1:?commit SHA}
[ -z "${2:-}" ] || { echo "usage: dev/release-gate.sh SHA" >&2; exit 2; }
state=$(gh api "repos/sadosystems/rargo2/commits/$commit/status" \
    --jq '[.statuses[] | select(.context == "gate/all")][0].state // "missing"')
[ "$state" = success ] && exit 0
echo "Release gate for $commit is $state. Run dev/gate.sh on that commit." >&2
exit 1
