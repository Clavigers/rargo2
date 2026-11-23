#!/bin/sh
# Starts or stops the development rargod on this machine.
#   dev/rargod.sh start [extra rargod options]
#   dev/rargod.sh stop
# The daemon runs in a transient systemd scope with a CPU and memory cap so
# the other services on the box keep headroom (see CLAUDE.md).
set -eu
work=${RARGO_WORK:-/root/rargo-work}
state=${RARGO_STATE:-$work/state}
repo=$(cd "$(dirname "$0")/.." && pwd)
case "${1:-}" in
start)
    shift
    mkdir -p "$state"
    if [ -S "$state/rargod.sock" ] && pgrep -f "rargod --state $state" >/dev/null 2>&1; then
        echo "rargod already running"; exit 0
    fi
    . "$work/env.sh"
    nohup systemd-run --scope -q --unit "rargod-dev-$$" -p CPUQuota=2400% -p MemoryMax=90G -p Delegate=yes \
        "${RARGO_BIN:-$repo/target/release}/rargod" --state "$state" \
        --planner "${RARGO_PLANNER:-$work/cargo/target/release/cargo}" --jobs 24 "$@" \
        >"$state/rargod.log" 2>&1 &
    for i in $(seq 1 100); do
        [ -S "$state/rargod.sock" ] && { echo "rargod started (log: $state/rargod.log)"; exit 0; }
        sleep 0.1
    done
    echo "rargod did not start:"; tail -20 "$state/rargod.log"; exit 1
    ;;
stop)
    pkill -TERM -f "rargod --state $state" 2>/dev/null || true
    for i in $(seq 1 50); do
        pgrep -f "rargod --state $state" >/dev/null 2>&1 || { echo "rargod stopped"; exit 0; }
        sleep 0.1
    done
    pkill -KILL -f "rargod --state $state" 2>/dev/null || true
    umount -l "$state/mnt" 2>/dev/null || true
    echo "rargod killed"
    ;;
*)
    echo "usage: $0 start|stop"; exit 2 ;;
esac
