#!/bin/bash
set -u
work=${RARGO_WORK:-/root/rargo-work}
here=$(cd "$(dirname "$0")" && pwd)
export RARGO_BIN=${RARGO_BIN:-$work/dev-target/release}
repo=${1:-ra}; agents=${2:-20}; c=${CORES:-8}
. "$work/env.sh"
. "$here/bench-lib.sh"
export RARGO_HIDE=$work/swarm
run() {
    . "$T/client.env"
    python3 "$here/swarm.py" --repo "$repo" --mode rargo --agents "$agents" --label "worker-swarm $1" \
        --out "$work/bench/results.jsonl"
    "$RARGO_BIN/rargo" --rargo-status | grep -A2 '^workers' | tr -s ' ' | head -c 1400; echo
    mkdir -p "$work/bench/logs"
    for f in srv w1; do
        [ -f "$T/$f.log" ] && cp "$T/$f.log" "$work/bench/logs/worker-swarm-${1// /_}-$f.log"
    done
}
bench_up $c $c 0 0;                run "local-$c"
bench_up $c $c 1 0;                run "$c+$c"
bench_up $((2 * c)) $c 0 0;        run "local-$((2 * c))"
bench_down
