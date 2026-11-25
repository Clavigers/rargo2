#!/bin/sh
set -eu
work=${RARGO_WORK:-/root/rargo-work}
B=${RARGO_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/release}
W=$work/wtest
case "${1:-}" in
start)
    "$0" stop >/dev/null 2>&1 || true
    rm -rf "$W"; mkdir -p "$W/srv" "$W/wrk"
    . "$work/env.sh"
    nohup "$B/rargod" --state "$W/srv" --planner "$work/cargo/target/release/cargo" --jobs "$2" \
        --workers-listen 127.0.0.1:7711 --drift-interval-min 0 >"$W/srv.log" 2>&1 &
    sleep 1
    nohup "$B/rargod" --state "$W/wrk" --worker 127.0.0.1:7711 --jobs "$3" >"$W/wrk.log" 2>&1 &
    sleep 1.5
    cat "$W/srv.log" "$W/wrk.log"
    ;;
stop)
    pkill -TERM -f "rargod --state $W" 2>/dev/null || true
    sleep 0.5
    pkill -KILL -f "rargod --state $W" 2>/dev/null || true
    umount -l "$W/srv/mnt" "$W/wrk/mnt" 2>/dev/null || true
    ;;
esac
