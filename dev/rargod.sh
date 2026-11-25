#!/bin/sh
set -eu
work=${RARGO_WORK:-/root/rargo-work}
state=${RARGO_STATE:-$work/state}
addr=${RARGO_SERVER:-127.0.0.1:7459}
repo=$(cd "$(dirname "$0")/.." && pwd)
case "${1:-}" in
start)
    shift
    mkdir -p "$state"
    if pgrep -f "rargod --state $state " >/dev/null 2>&1; then
        echo "rargod already running"; exit 0
    fi
    . "$work/env.sh"
    export RUSTUP_HOME="${RARGO_SERVER_RUSTUP_HOME:-$RUSTUP_HOME}"
    : >"$state/rargod.log"
    hide=""
    for d in ${RARGO_HIDE:-}; do mkdir -p "$d"; hide="$hide mount -t tmpfs tmpfs '$d' &&"; done
    nohup systemd-run --scope -q --unit "rargod-dev-$$" -p "CPUQuota=${RARGO_CPU_QUOTA:-2400%}" -p "MemoryMax=${RARGO_MEMORY_MAX:-90G}" -p Delegate=yes \
        unshare -m --propagation private sh -c "$hide exec \"\$@\"" sh \
        "${RARGO_BIN:-$repo/target/release}/rargod" --state "$state" --listen "$addr" \
        --planner "${RARGO_PLANNER:-$work/cargo/target/release/cargo}" \
        --cargo-home "${RARGO_SERVER_CARGO_HOME:-$work/cargo-home}" --jobs "${RARGO_JOBS:-24}" "$@" \
        >>"$state/rargod.log" 2>&1 &
    srv=$!
    if tail -n +1 -f --pid="$srv" "$state/rargod.log" | awk '/serving clients/ { found=1; exit } END { exit found ? 0 : 1 }'; then
        echo "rargod started on $addr (log: $state/rargod.log)"
        exit 0
    fi
    echo "rargod did not start:"; tail -20 "$state/rargod.log"; exit 1
    ;;
stop)
    bin=$(realpath -m "${RARGO_BIN:-$repo/target/release}/rargo")
    cache=${RARGO_CACHE_DIR:-${XDG_CACHE_HOME:-${HOME%/}/.cache}/rargo}
    pattern=$(printf '%s' "$bin --rargo-daemon $cache/run/" | sed 's/[][(){}.^$*+?|\\]/\\&/g')
    dpids=$(pgrep -f -- "^$pattern" || true)
    spids=$(pgrep -f "rargod --state $state " || true)
    pkill -TERM -f -- "^$pattern" 2>/dev/null || true
    pkill -TERM -f "rargod --state $state " 2>/dev/null || true
    for pid in $dpids $spids; do
        tail --pid="$pid" -f /dev/null
    done
    if pgrep -f "rargod --state $state " >/dev/null 2>&1; then
        left=$(pgrep -f "rargod --state $state " || true)
        pkill -KILL -f "rargod --state $state " 2>/dev/null || true
        for pid in $left; do
            tail --pid="$pid" -f /dev/null
        done
        umount -l "$state/mnt" 2>/dev/null || true
        echo "rargod killed"
    else
        echo "rargod stopped"
    fi
    ;;
*)
    echo "usage: $0 start|stop"; exit 2 ;;
esac
