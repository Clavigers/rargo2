#!/bin/bash
# The 20-agent swarm (dev/swarm.py), its clients in their own network
# namespace, served by a server in another, without and with a worker
# behind a netem link (1 ms each way, 0.1% loss, 1 Gbit): the phase 4
# criterion on a workload with parallelism across actions.
#   [CORES=8] dev/bench-worker-swarm.sh [REPO] [AGENTS]
# Configurations: local-C, C+C (one fresh worker), local-2C.
# Results go to $RARGO_WORK/bench/results.jsonl, labelled "worker-swarm ...".
set -u
work=${RARGO_WORK:-/root/rargo-work}
here=$(cd "$(dirname "$0")" && pwd)
export RARGO_BIN=${RARGO_BIN:-$work/dev-target/release}
repo=${1:-ra}; agents=${2:-20}; c=${CORES:-8}
. "$work/env.sh"
export RARGO_HIDE=$work/swarm
run() { # label
    . "$work/nstest/client.env"
    python3 "$here/swarm.py" --repo "$repo" --mode rargo --agents "$agents" --label "worker-swarm $1" \
        --netns rargo-cl --out "$work/bench/results.jsonl"
    nsenter --net=/run/netns/rargo-cl "$RARGO_BIN/rargo" --rargo-status | grep -A2 '^workers' | tr -s ' ' | head -c 400; echo
}
"$here/netns.sh" up $c $c 0 >/dev/null 2>&1;            run "local-$c"
"$here/netns.sh" up $c $c 1 1 0.1 1gbit >/dev/null 2>&1; run "$c+$c"
"$here/netns.sh" up $((2 * c)) $c 0 >/dev/null 2>&1;    run "local-$((2 * c))"
"$here/netns.sh" down >/dev/null 2>&1
