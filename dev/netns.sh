#!/bin/sh
# A server and workers in separate network namespaces on one machine, joined
# by veth pairs with injected latency, loss and a rate limit (tc netem).
# Nothing is configured in the host's own network namespace.
#
#   dev/netns.sh up SERVER_JOBS WORKER_JOBS [N_WORKERS] [DELAY_MS] [LOSS_PCT] [RATE]
#   dev/netns.sh reset-server     (fresh server state, workers keep theirs)
#   dev/netns.sh down
#
# The server's client socket is $RARGO_WORK/nstest/srv/rargod.sock.
set -eu
work=${RARGO_WORK:-/root/rargo-work}
B=${RARGO_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/release}
T=$work/nstest
down() {
    pkill -TERM -f "rargod --state $T" 2>/dev/null || true
    sleep 0.5
    pkill -KILL -f "rargod --state $T" 2>/dev/null || true
    for ns in $(ip netns list 2>/dev/null | awk '/^rargo-/ {print $1}'); do
        ip netns del "$ns" 2>/dev/null || true
    done
}
start_server() {
    mkdir -p "$T/srv"
    nohup systemd-run --scope -q -p CPUQuota=$((sj * 100))% -p MemoryMax=60G \
        ip netns exec rargo-srv "$B/rargod" --state "$T/srv" --planner "$work/cargo/target/release/cargo" \
        --jobs "$sj" --workers-listen 10.77.0.1:7711 --worker-key "$T/link.key" --drift-interval-min 0 >"$T/srv.log" 2>&1 &
    sleep 1
}
start_workers() {
    i=1
    while [ "$i" -le "$n" ]; do
        mkdir -p "$T/w$i"
        nohup systemd-run --scope -q -p CPUQuota=$((wj * 100))% -p MemoryMax=30G \
            ip netns exec "rargo-w$i" "$B/rargod" --state "$T/w$i" --worker 10.77.0.1:7711 --worker-key "$T/link.key" --jobs "$wj" \
            >"$T/w$i.log" 2>&1 &
        i=$((i + 1))
    done
}
case "${1:-}" in
up)
    sj=$2; wj=$3; n=${4:-1}; delay=${5:-1}; loss=${6:-0.1}; rate=${7:-1gbit}
    down
    rm -rf "$T"; mkdir -p "$T/srv"
    . "$work/env.sh"
    ip netns add rargo-srv
    ip -n rargo-srv link set lo up
    # One bridge in the server's namespace; every worker hangs off it.
    ip -n rargo-srv link add br0 type bridge
    ip -n rargo-srv addr add 10.77.0.1/24 dev br0
    ip -n rargo-srv link set br0 up
    i=1
    while [ "$i" -le "$n" ]; do
        ns=rargo-w$i
        ip netns add "$ns"
        ip -n "$ns" link set lo up
        ip link add "vs$i" netns rargo-srv type veth peer name eth0 netns "$ns"
        ip -n rargo-srv link set "vs$i" master br0
        ip -n rargo-srv link set "vs$i" up
        ip -n "$ns" addr add "10.77.0.$((i + 1))/24" dev eth0
        ip -n "$ns" link set eth0 up
        # Shape both directions of the link.
        ip netns exec rargo-srv tc qdisc add dev "vs$i" root netem delay "${delay}ms" loss "${loss}%" rate "$rate"
        ip netns exec "$ns" tc qdisc add dev eth0 root netem delay "${delay}ms" loss "${loss}%" rate "$rate"
        i=$((i + 1))
    done
    head -c 32 /dev/urandom >"$T/link.key"
    echo "sj=$sj wj=$wj n=$n" >"$T/params"
    start_server
    start_workers
    sleep 2
    cat "$T/srv.log"
    [ "$n" -ge 1 ] && ip netns exec rargo-w1 ping -c 3 -q 10.77.0.1 | tail -2
    true
    ;;
reset-server)
    # A fresh server (empty store and cache) with the same workers, which
    # keep what they hold: the cold build of a server whose workers are warm.
    . "$T/params"
    . "$work/env.sh"
    pkill -TERM -f "rargod --state $T" 2>/dev/null || true
    sleep 1
    pkill -KILL -f "rargod --state $T" 2>/dev/null || true
    umount -l "$T/srv/mnt" 2>/dev/null || true
    rm -rf "$T/srv"
    i=1
    while [ "$i" -le "$n" ]; do umount -l "$T/w$i/mnt" 2>/dev/null || true; i=$((i + 1)); done
    start_server
    start_workers
    sleep 2
    cat "$T/srv.log"
    ;;
down)
    down
    ;;
*)
    echo "usage: $0 up SERVER_JOBS WORKER_JOBS [N_WORKERS] [DELAY_MS] [LOSS_PCT] [RATE] | reset-server | down"; exit 2 ;;
esac
