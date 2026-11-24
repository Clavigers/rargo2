#!/bin/bash
# The release and local deployment both require the gate on this exact SHA.
set -euo pipefail
commit=${1:?commit SHA}
state=$(gh api "repos/sadosystems/rargo2/commits/$commit/status" \
    --jq '[.statuses[] | select(.context == "gate/all")][0].state // "missing"')
if [ "$state" != success ]; then
    echo "Release gate for $commit is $state. Run dev/gate.sh on that commit." >&2
    exit 1
fi
