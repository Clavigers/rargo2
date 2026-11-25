#!/bin/bash
set -euo pipefail
wait=false
if [ "${1:-}" = --wait ]; then wait=true; shift; fi
commit=${1:?usage: dev/ci-passed.sh [--wait] SHA}
[ -z "${2:-}" ] || { echo "usage: dev/ci-passed.sh [--wait] SHA" >&2; exit 2; }
for _ in $(seq 1 240); do
    state=$(gh api "repos/sadosystems/rargo2/commits/$commit/check-runs?check_name=build-and-test" \
        --jq '[.check_runs[] | .conclusion // .status][0] // "missing"')
    case "$state" in
        success) exit 0 ;;
        queued|in_progress|pending|waiting|requested|missing) "$wait" || break; sleep 10 ;;
        *) break ;;
    esac
done
echo "CI (build-and-test) for $commit is $state." >&2
exit 1
