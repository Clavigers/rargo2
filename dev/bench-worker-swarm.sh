#!/bin/bash
set -u
work=${RARGO_WORK:-/root/rargo-work}
here=$(cd "$(dirname "$0")" && pwd)
export RARGO_BIN=${RARGO_BIN:-$work/dev-target/release}
repo=${1:-ra}; agents=${2:-20}; c=${CORES:-8}
. "$work/env.sh"
export RARGO_HIDE=$work/swarm
run() {
    . "$work/${RIG_STATE:-nstest}/client.env"
    python3 "$here/swarm.py" --repo "$repo" --mode rargo --agents "$agents" --label "worker-swarm $1" \
        --netns ${RIG:-rargo}-cl --out "$work/bench/results.jsonl"
    nsenter --net=/run/netns/${RIG:-rargo}-cl "$RARGO_BIN/rargo" --rargo-status | grep -A2 '^workers' | tr -s ' ' | head -c 1400; echo
    mkdir -p "$work/bench/logs"
    for f in srv w1; do
        [ -f "$work/${RIG_STATE:-nstest}/$f.log" ] && cp "$work/${RIG_STATE:-nstest}/$f.log" "$work/bench/logs/worker-swarm-${1// /_}-$f.log"
    done
}
"$here/netns.sh" up $c $c 0 >/dev/null 2>&1;            run "local-$c"
"$here/netns.sh" up $c $c 1 1 0.1 1gbit >/dev/null 2>&1; run "$c+$c"
"$here/netns.sh" up $((2 * c)) $c 0 >/dev/null 2>&1;    run "local-$((2 * c))"
"$here/netns.sh" down >/dev/null 2>&1
