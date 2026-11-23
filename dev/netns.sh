#!/bin/sh
# A server, its workers and a client machine in separate network namespaces
# on one machine, joined by veth pairs with injected latency, loss and a
# rate limit (tc netem). Nothing is configured in the host's own network
# namespace.
#
#   dev/netns.sh up SERVER_JOBS WORKER_JOBS [N_WORKERS] [DELAY_MS] [LOSS_PCT] [RATE]
#   dev/netns.sh reset-server     (fresh server state, workers keep theirs)
#   dev/netns.sh down
#
# The client's link is shaped separately: CLIENT_DELAY_MS (each way, default
# 1), CLIENT_LOSS_PCT (default 0) and CLIENT_RATE (default 1gbit).
# Clients run as `ip netns exec rargo-cl` with the variables in
# $RARGO_WORK/nstest/client.env; the server cannot see the directories named
# in RARGO_HIDE.
set -eu
# netem keeps every packet it is delaying in one queue, 1000 packets long
# unless told otherwise: less than a 1 Gbit link has in flight at 20 ms of
# delay, so the emulated link would drop what a real one carries. (TCP is
# spared only because its segments are queued in batches.) The queue is
# sized for the fastest, longest link used here.
qlimit=${NETEM_LIMIT:-100000}
work=${RARGO_WORK:-/root/rargo-work}
B=${RARGO_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/release}
T=$work/nstest
down() {
    # Client daemons started with these binaries (they live in the
    # client's namespace).
    pkill -TERM -f "$B/rargo --rargo-daemon" 2>/dev/null || true
    pkill -TERM -f "rargod --state $T" 2>/dev/null || true
    pkill -TERM -f "rigproxy.py [a-z]* $T/proxy.sock" 2>/dev/null || true
    sleep 0.5
    pkill -KILL -f "rargod --state $T" 2>/dev/null || true
    for ns in $(ip netns list 2>/dev/null | awk '/^rargo-/ {print $1}'); do
        ip netns del "$ns" 2>/dev/null || true
    done
}
start_server() {
    mkdir -p "$T/srv"
    # RIG_PROXY=1: the server namespace has no route out; build scripts the
    # policy lets use the network get out through a proxy (dev/rigproxy.py)
    # at HTTPS_PROXY=http://127.0.0.1:3128.
    if [ "${RIG_PROXY:-}" = 1 ] && ! pgrep -f "rigproxy.py outside $T/proxy.sock" >/dev/null 2>&1; then
        here=$(cd "$(dirname "$0")" && pwd)
        nohup python3 "$here/rigproxy.py" outside "$T/proxy.sock" >/dev/null 2>&1 &
        nohup ip netns exec rargo-srv python3 "$here/rigproxy.py" inside "$T/proxy.sock" 3128 >/dev/null 2>&1 &
    fi
    hide=""
    for d in ${RARGO_HIDE:-}; do mkdir -p "$d"; hide="$hide mount -t tmpfs tmpfs '$d' &&"; done
    nohup systemd-run --scope -q -p CPUQuota=$((sj * 100))% -p MemoryMax=60G -p Delegate=yes \
        ip netns exec rargo-srv unshare -m --propagation private sh -c "$hide exec \"\$@\"" sh \
        "$B/rargod" --state "$T/srv" --planner "$work/cargo/target/release/cargo" \
        --listen 10.77.0.1:7459 --keys "$T/keys" --cargo-home "$work/cargo-home" \
        --jobs "$sj" --workers-listen 10.77.0.1:7711 --worker-key "$T/link.key" --drift-interval-min 0 >"$T/srv.log" 2>&1 &
    for i in $(seq 1 100); do
        grep -q "serving clients" "$T/srv.log" 2>/dev/null && break
        sleep 0.1
    done
}
start_workers() {
    i=1
    while [ "$i" -le "$n" ]; do
        mkdir -p "$T/w$i"
        nohup systemd-run --scope -q -p CPUQuota=$((wj * 100))% -p MemoryMax=30G -p Delegate=yes \
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
        ip netns exec rargo-srv tc qdisc add dev "vs$i" root netem delay "${delay}ms" loss "${loss}%" rate "$rate" limit "$qlimit"
        ip netns exec "$ns" tc qdisc add dev eth0 root netem delay "${delay}ms" loss "${loss}%" rate "$rate" limit "$qlimit"
        i=$((i + 1))
    done
    # The client machine.
    ip netns add rargo-cl
    ip -n rargo-cl link set lo up
    ip link add vcl netns rargo-srv type veth peer name eth0 netns rargo-cl
    ip -n rargo-srv link set vcl master br0
    ip -n rargo-srv link set vcl up
    ip -n rargo-cl addr add 10.77.0.200/24 dev eth0
    ip -n rargo-cl link set eth0 up
    cdelay=${CLIENT_DELAY_MS:-1}; closs=${CLIENT_LOSS_PCT:-0}; crate=${CLIENT_RATE:-1gbit}
    ip netns exec rargo-srv tc qdisc add dev vcl root netem delay "${cdelay}ms" loss "${closs}%" rate "$crate" limit "$qlimit"
    ip netns exec rargo-cl tc qdisc add dev eth0 root netem delay "${cdelay}ms" loss "${closs}%" rate "$crate" limit "$qlimit"
    head -c 32 /dev/urandom >"$T/link.key"
    secret=$(head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')
    echo "bench $secret admin" >"$T/keys"
    printf 'export RARGO_SERVER=10.77.0.1:7459 RARGO_USER=bench RARGO_KEY=%s RARGO_STATE=%s\n' "$secret" "$T/srv" >"$T/client.env"
    echo "sj=$sj wj=$wj n=$n RARGO_HIDE='${RARGO_HIDE:-}' RIG_PROXY='${RIG_PROXY:-}'" >"$T/params"
    start_server
    start_workers
    sleep 2
    cat "$T/srv.log"
    [ "$n" -ge 1 ] && ip netns exec rargo-w1 ping -c 3 -q 10.77.0.1 | tail -2
    ip netns exec rargo-cl ping -c 3 -q 10.77.0.1 | tail -1
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
