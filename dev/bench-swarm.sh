#!/bin/sh
set -eu
work=${RARGO_WORK:-/root/rargo-work}
repo=$(cd "$(dirname "$0")/.." && pwd)
name=$1
agents=${2:-20}
. "$work/env.sh"
out=$work/bench/results.jsonl
jobs=${RARGO_JOBS:-24}
renv=""
[ "$name" = zed ] && export GIT_LFS_SKIP_SMUDGE=1 RIG_PROXY=1 && renv="HTTPS_PROXY=http://127.0.0.1:3128"
tests=${SWARM_TESTS:-test}
nextest=$work/tools/nextest-musl/${NEXTEST_VERSION:-0.9.152}
if [ "$tests" = nextest ] && [ ! -x "$nextest/cargo-nextest" ]; then
    echo "no nextest at $nextest" >&2
    exit 2
fi
if [ "${SWARM_SKIP_RARGO:-}" != 1 ]; then
    RARGO_HIDE="$work/swarm" "$repo/dev/netns.sh" up "$jobs" 0 0 >/dev/null
    . "$work/${RIG_STATE:-nstest}/client.env"
    if [ "$tests" = nextest ]; then
        v=$(basename "$nextest")
        mkdir -p "$RARGO_STATE/nextest/$v"
        cp "$nextest/cargo-nextest" "$RARGO_STATE/nextest/$v/"
        echo "$v" >"$RARGO_STATE/nextest/newest-release"
    fi
    env $renv python3 "$repo/dev/swarm.py" --repo "$name" --mode rargo --agents "$agents" --out "$out" --netns ${RIG:-rargo}-cl \
        --tests "$tests" --label "remote client ${CLIENT_DELAY_MS:-1}ms ${CLIENT_RATE:-1gbit}"
    "$repo/dev/netns.sh" down
fi
if [ "${SWARM_SKIP_STOCK:-}" != 1 ]; then
    [ "$tests" = nextest ] && export PATH="$nextest:$PATH"
    systemd-run --scope -q -p CPUQuota=$((jobs * 100))% -p MemoryMax=90G \
        env PATH="$PATH" python3 "$repo/dev/swarm.py" --repo "$name" --mode stock --agents "$agents" --out "$out" --tests "$tests" \
        ${STOCK_BATCH:+--batch "$STOCK_BATCH" --label "stock cargo, agents in groups of $STOCK_BATCH"}
fi
