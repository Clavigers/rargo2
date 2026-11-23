#!/bin/bash
# The 20-agent swarm with rargo's rustc fork against the stock compiler, all
# under rargo, with a body-only second edit (dev/swarm.py --edit body):
#   stable          stock rustc 1.100.0
#   fork            the fork, check planned as a build (the default with it)
#   fork-cargo-check  the fork, check the cargo way (RARGO_CHECK_AS_BUILD=0)
# Results go to $RARGO_WORK/bench/results.jsonl with these labels.
#   dev/bench-fork.sh [REPO] [AGENTS]
set -eu
work=${RARGO_WORK:-/root/rargo-work}
here=$(cd "$(dirname "$0")" && pwd)
repo=${1:-ra}; agents=${2:-20}
. "$work/env.sh"
export RARGO_BIN=${RARGO_BIN:-$work/dev-target/release}
export RARGO_STATE=$work/state-fork-bench
out=$work/bench/results.jsonl
run() { # label toolchain check-as-build
    "$here/rargod.sh" stop >/dev/null 2>&1 || true
    rm -rf "$RARGO_STATE"
    "$here/rargod.sh" start >/dev/null
    RARGO_SOCKET=$RARGO_STATE/rargod.sock RARGO_CHECK_AS_BUILD=$3 \
        python3 "$here/swarm.py" --repo "$repo" --mode rargo --agents "$agents" --edit body \
        --toolchain "$2" --label "$1" --out "$out"
    "$here/rargod.sh" stop >/dev/null 2>&1 || true
}
run stable stable 1
run fork rargo 1
run fork-cargo-check rargo 0
