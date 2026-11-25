#!/bin/bash
set -eu
work=${RARGO_WORK:-/root/rargo-work}
here=$(cd "$(dirname "$0")" && pwd)
repo=${1:-ra}; agents=${2:-20}
. "$work/env.sh"
export RARGO_BIN=${RARGO_BIN:-$work/dev-target/release}
export RARGO_STATE=$work/state-fork-bench
export RARGO_SERVER=127.0.0.1:7473
export RARGO_HIDE=$work/swarm
out=$work/bench/results.jsonl
run() {
    "$here/rargod.sh" stop >/dev/null 2>&1 || true
    rm -rf "$RARGO_STATE"
    "$here/rargod.sh" start >/dev/null
    RARGO_CHECK_AS_BUILD=$3 \
        python3 "$here/swarm.py" --repo "$repo" --mode rargo --agents "$agents" --edit body \
        --toolchain "$2" --label "$1" --out "$out"
    "$here/rargod.sh" stop >/dev/null 2>&1 || true
}
run stable stable 1
run fork rargo 1
run fork-cargo-check rargo 0
