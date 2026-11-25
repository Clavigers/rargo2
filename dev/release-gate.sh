#!/bin/bash
# The release and local deployment both require the gate on this exact SHA.
set -euo pipefail
commit=${1:?commit SHA}
attempts=1
[ "${2:-}" != --wait ] || attempts=15
for ((attempt = 1; attempt <= attempts; attempt++)); do
    state=$(gh api "repos/sadosystems/rargo2/commits/$commit/status" \
        --jq '[.statuses[] | select(.context == "gate/all")][0].state // "missing"')
    case "$state" in
        success) exit 0 ;;
        pending|missing) [ "$attempt" -eq "$attempts" ] || sleep 2 ;;
        *) break ;;
    esac
done
echo "Release gate for $commit is $state. Run dev/gate.sh on that commit." >&2
exit 1
