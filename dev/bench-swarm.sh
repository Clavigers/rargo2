#!/bin/sh
# Runs the 20-agent swarm benchmark for one repository in both modes.
#   dev/bench-swarm.sh ra|ruff|bevy|zed [agents]
# The rargo agents are clients in their own network namespace, one shaped
# link away from the build server (1 ms each way, 1 Gbit; change with
# CLIENT_DELAY_MS and CLIENT_RATE), and the server cannot see their
# worktrees. Results are appended to $RARGO_WORK/bench/results.jsonl.
set -eu
work=${RARGO_WORK:-/root/rargo-work}
repo=$(cd "$(dirname "$0")/.." && pwd)
name=$1
agents=${2:-20}
. "$work/env.sh"
out=$work/bench/results.jsonl
jobs=${RARGO_JOBS:-24}
# One of Zed's git dependencies has LFS files neither build needs.
# One of its build scripts downloads a library: the rig's server gets a
# proxy out, named to the build through the policy file's `env.pass`.
renv=""
[ "$name" = zed ] && export GIT_LFS_SKIP_SMUDGE=1 RIG_PROXY=1 && renv="HTTPS_PROXY=http://127.0.0.1:3128"
if [ "${SWARM_SKIP_RARGO:-}" != 1 ]; then
    # rargo: fresh server and store, so nothing is warm.
    RARGO_HIDE="$work/swarm" "$repo/dev/netns.sh" up "$jobs" 0 0 >/dev/null
    . "$work/nstest/client.env"
    env $renv python3 "$repo/dev/swarm.py" --repo "$name" --mode rargo --agents "$agents" --out "$out" --netns rargo-cl \
        --label "remote client ${CLIENT_DELAY_MS:-1}ms ${CLIENT_RATE:-1gbit}"
    "$repo/dev/netns.sh" down
fi
if [ "${SWARM_SKIP_STOCK:-}" != 1 ]; then
    # stock: same CPU and memory cap as the server gets.
    systemd-run --scope -q -p CPUQuota=$((jobs * 100))% -p MemoryMax=90G \
        python3 "$repo/dev/swarm.py" --repo "$name" --mode stock --agents "$agents" --out "$out" \
        ${STOCK_BATCH:+--batch "$STOCK_BATCH" --label "stock cargo, agents in groups of $STOCK_BATCH"}
fi
