#!/bin/sh
# Starts or stops the development build server on this machine.
#   dev/rargod.sh start [extra rargod options]
#   dev/rargod.sh stop
# The server runs in a transient systemd scope with a CPU and memory cap so
# the other services on the box keep headroom (see CLAUDE.md).
# Override the limits with RARGO_CPU_QUOTA and RARGO_MEMORY_MAX, the address
# with RARGO_SERVER (default 127.0.0.1:7459). RARGO_HIDE names directories
# the server must not see; RARGO_SERVER_RUSTUP_HOME gives it toolchains of
# its own.
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
    # RARGO_SERVER_RUSTUP_HOME: toolchains of the server's own, instead of
    # the shared ones.
    export RUSTUP_HOME="${RARGO_SERVER_RUSTUP_HOME:-$RUSTUP_HOME}"
    : >"$state/rargod.log"
    # RARGO_HIDE: directories (space separated) the server must not be able
    # to see, e.g. the client worktrees of a test on the same machine. The
    # server then runs in its own mount namespace with an empty file system
    # over each of them.
    hide=""
    for d in ${RARGO_HIDE:-}; do mkdir -p "$d"; hide="$hide mount -t tmpfs tmpfs '$d' &&"; done
    nohup systemd-run --scope -q --unit "rargod-dev-$$" -p "CPUQuota=${RARGO_CPU_QUOTA:-2400%}" -p "MemoryMax=${RARGO_MEMORY_MAX:-90G}" -p Delegate=yes \
        unshare -m --propagation private sh -c "$hide exec \"\$@\"" sh \
        "${RARGO_BIN:-$repo/target/release}/rargod" --state "$state" --listen "$addr" \
        --planner "${RARGO_PLANNER:-$work/cargo/target/release/cargo}" \
        --cargo-home "${RARGO_SERVER_CARGO_HOME:-$work/cargo-home}" --jobs "${RARGO_JOBS:-24}" "$@" \
        >>"$state/rargod.log" 2>&1 &
    for i in $(seq 1 100); do
        grep -q "serving clients" "$state/rargod.log" 2>/dev/null && { echo "rargod started on $addr (log: $state/rargod.log)"; exit 0; }
        sleep 0.1
    done
    echo "rargod did not start:"; tail -20 "$state/rargod.log"; exit 1
    ;;
stop)
    # Match this test's cache as well as its binary. Parallel suites
    # can use the same binaries with separate caches and servers.
    bin=$(realpath -m "${RARGO_BIN:-$repo/target/release}/rargo")
    cache=${RARGO_CACHE_DIR:-${XDG_CACHE_HOME:-${HOME%/}/.cache}/rargo}
    pattern=$(printf '%s' "$bin --rargo-daemon $cache/run/" | sed 's/[][(){}.^$*+?|\\]/\\&/g')
    pkill -TERM -f -- "^$pattern" 2>/dev/null || true
    pkill -TERM -f "rargod --state $state " 2>/dev/null || true
    for i in $(seq 1 50); do
        pgrep -f "rargod --state $state " >/dev/null 2>&1 || { echo "rargod stopped"; exit 0; }
        sleep 0.1
    done
    pkill -KILL -f "rargod --state $state " 2>/dev/null || true
    umount -l "$state/mnt" 2>/dev/null || true
    echo "rargod killed"
    ;;
*)
    echo "usage: $0 start|stop"; exit 2 ;;
esac
