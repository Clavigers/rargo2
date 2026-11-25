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
# Clients run as `nsenter --net=/run/netns/rargo-cl` (their network
# namespace only: the mount namespace stays the host's, where the client
# daemon's output directories are mounted) with the variables in
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
# RIG names the rig (its namespaces are $RIG-srv, $RIG-cl, $RIG-wN) and
# RIG_STATE its directory under $RARGO_WORK, so that a second rig can run
# beside one that is in use.
RIG=${RIG:-rargo}
# (an absolute RIG_STATE is used as it is).
case ${RIG_STATE:-nstest} in /*) T=$RIG_STATE ;; *) T=$work/${RIG_STATE:-nstest} ;; esac
# Client daemons of this rig run in its client namespace. Other suites
# use the same binaries in namespaces of their own, so a signal goes
# only to a process whose network namespace is this rig's.
signal_client_daemons() { # TERM or KILL
    ns=/run/netns/$RIG-cl
    [ -e "$ns" ] || return 0
    ino=$(stat -c %i "$ns")
    for pid in $(pgrep -f "$B/rargo --rargo-daemon" 2>/dev/null || true); do
        if [ "$(stat -c %i "/proc/$pid/ns/net" 2>/dev/null || echo none)" = "$ino" ]; then
            kill "-$1" "$pid" 2>/dev/null || true
        fi
    done
}
# Pids of client daemons in this rig's client namespace, one per line.
client_daemon_pids() {
    ns=/run/netns/$RIG-cl
    [ -e "$ns" ] || return 0
    ino=$(stat -c %i "$ns")
    for pid in $(pgrep -f "$B/rargo --rargo-daemon" 2>/dev/null || true); do
        if [ "$(stat -c %i "/proc/$pid/ns/net" 2>/dev/null || echo none)" = "$ino" ]; then
            printf '%s\n' "$pid"
        fi
    done
}
# Block on each pid. A pid that has already exited returns at once.
wait_pids() {
    for pid in "$@"; do
        [ -n "$pid" ] || continue
        tail --pid="$pid" -f /dev/null
    done
}
# rargod of this rig has exited (its state directory is $T/...).
wait_servers_gone() {
    pids=$(pgrep -f "rargod --state $T/" || true)
    # shellcheck disable=SC2086
    wait_pids $pids
}
down() {
    cpids=$(client_daemon_pids | tr '\n' ' ')
    signal_client_daemons TERM
    spids=$(pgrep -f "rargod --state $T/" || true)
    ppids=$(pgrep -f "rigproxy.py [a-z]* $T/proxy.sock" || true)
    pkill -TERM -f "rargod --state $T/" 2>/dev/null || true
    pkill -TERM -f "rigproxy.py [a-z]* $T/proxy.sock" 2>/dev/null || true
    # shellcheck disable=SC2086
    wait_pids $cpids $spids $ppids
    spids=$(pgrep -f "rargod --state $T/" || true)
    pkill -KILL -f "rargod --state $T/" 2>/dev/null || true
    signal_client_daemons KILL
    cpids=$(client_daemon_pids | tr '\n' ' ')
    # shellcheck disable=SC2086
    wait_pids $spids $cpids
    for nsname in $(ip netns list 2>/dev/null | awk -v p="^$RIG-" '$1 ~ p {print $1}'); do
        ip netns del "$nsname" 2>/dev/null || true
    done
}
# The rig is up when the server is serving, any worker has joined, and
# the client can reach the server.
wait_up() {
    if [ "${n:-0}" -ge 1 ]; then
        if ! tail -n +1 -f --pid="$srvpid" "$T/srv.log" | awk '/joined/ { found=1; exit } END { exit found ? 0 : 1 }'; then
            echo "no worker joined" >&2
            return 1
        fi
    fi
    ip netns exec "$RIG-cl" ping -c 1 -W 2 -q 10.77.0.1 >/dev/null 2>&1
}
start_server() {
    mkdir -p "$T/srv"
    [ -f "$T/srv/tokens.json" ] || cp "$T/tokens.json" "$T/srv/tokens.json"
    # RIG_PROXY=1: the server namespace has no route out; build scripts the
    # policy lets use the network get out through a proxy (dev/rigproxy.py)
    # at HTTPS_PROXY=http://127.0.0.1:3128.
    if [ "${RIG_PROXY:-}" = 1 ] && ! pgrep -f "rigproxy.py outside $T/proxy.sock" >/dev/null 2>&1; then
        here=$(cd "$(dirname "$0")" && pwd)
        nohup python3 "$here/rigproxy.py" outside "$T/proxy.sock" >/dev/null 2>&1 &
        nohup ip netns exec $RIG-srv python3 "$here/rigproxy.py" inside "$T/proxy.sock" 3128 >/dev/null 2>&1 &
    fi
    hide=""
    for d in ${RARGO_HIDE:-}; do mkdir -p "$d"; hide="$hide mount -t tmpfs tmpfs '$d' &&"; done
    nohup systemd-run --scope -q -p CPUQuota=$((sj * 100))% -p MemoryMax=60G -p Delegate=yes \
        ip netns exec $RIG-srv unshare -m --propagation private sh -c "$hide exec \"\$@\"" sh \
        "$B/rargod" --state "$T/srv" --planner "$work/cargo/target/release/cargo" \
        --listen 10.77.0.1:7459 --cargo-home "$work/cargo-home" \
        --jobs "$sj" --workers-listen 10.77.0.1:7711 --worker-key "$T/link.key" --drift-interval-min 0 >"$T/srv.log" 2>&1 &
    srvpid=$!
    if ! tail -n +1 -f --pid="$srvpid" "$T/srv.log" | awk '/serving clients/ { found=1; exit } END { exit found ? 0 : 1 }'; then
        echo "rargod did not start" >&2
        return 1
    fi
}
start_workers() {
    i=1
    while [ "$i" -le "$n" ]; do
        mkdir -p "$T/w$i"
        nohup systemd-run --scope -q -p CPUQuota=$((wj * 100))% -p MemoryMax=30G -p Delegate=yes \
            ip netns exec "$RIG-w$i" "$B/rargod" --state "$T/w$i" --worker 10.77.0.1:7711 --worker-key "$T/link.key" --jobs "$wj" \
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
    ip netns add $RIG-srv
    ip -n $RIG-srv link set lo up
    # One bridge in the server's namespace; every worker hangs off it.
    ip -n $RIG-srv link add br0 type bridge
    ip -n $RIG-srv addr add 10.77.0.1/24 dev br0
    ip -n $RIG-srv link set br0 up
    i=1
    while [ "$i" -le "$n" ]; do
        ns=$RIG-w$i
        ip netns add "$ns"
        ip -n "$ns" link set lo up
        ip link add "vs$i" netns $RIG-srv type veth peer name eth0 netns "$ns"
        ip -n $RIG-srv link set "vs$i" master br0
        ip -n $RIG-srv link set "vs$i" up
        ip -n "$ns" addr add "10.77.0.$((i + 1))/24" dev eth0
        ip -n "$ns" link set eth0 up
        # Shape both directions of the link.
        ip netns exec $RIG-srv tc qdisc add dev "vs$i" root netem delay "${delay}ms" loss "${loss}%" rate "$rate" limit "$qlimit"
        ip netns exec "$ns" tc qdisc add dev eth0 root netem delay "${delay}ms" loss "${loss}%" rate "$rate" limit "$qlimit"
        i=$((i + 1))
    done
    # The client machine.
    ip netns add $RIG-cl
    ip -n $RIG-cl link set lo up
    ip link add vcl netns $RIG-srv type veth peer name eth0 netns $RIG-cl
    ip -n $RIG-srv link set vcl master br0
    ip -n $RIG-srv link set vcl up
    ip -n $RIG-cl addr add 10.77.0.200/24 dev eth0
    ip -n $RIG-cl link set eth0 up
    cdelay=${CLIENT_DELAY_MS:-1}; closs=${CLIENT_LOSS_PCT:-0}; crate=${CLIENT_RATE:-1gbit}
    ip netns exec $RIG-srv tc qdisc add dev vcl root netem delay "${cdelay}ms" loss "${closs}%" rate "$crate" limit "$qlimit"
    ip netns exec $RIG-cl tc qdisc add dev eth0 root netem delay "${cdelay}ms" loss "${closs}%" rate "$crate" limit "$qlimit"
    head -c 32 /dev/urandom >"$T/link.key"
    # The rig's one user: a token, kept beside the server's state so that
    # a server started on an emptied state directory admits it too.
    token=$("$B/rargod" token create --state "$T/srv" --user bench --admin 2>/dev/null)
    cp "$T/srv/tokens.json" "$T/tokens.json"
    printf 'export RARGO_SERVER=10.77.0.1:7459 RARGO_TOKEN=%s RARGO_STATE=%s\n' "$token" "$T/srv" >"$T/client.env"
    echo "sj=$sj wj=$wj n=$n RARGO_HIDE='${RARGO_HIDE:-}' RIG_PROXY='${RIG_PROXY:-}'" >"$T/params"
    start_server
    start_workers
    wait_up
    cat "$T/srv.log"
    [ "$n" -ge 1 ] && ip netns exec $RIG-w1 ping -c 3 -q 10.77.0.1 | tail -2
    ip netns exec $RIG-cl ping -c 3 -q 10.77.0.1 | tail -1
    true
    ;;
reset-server)
    # A fresh server (empty store and cache) with the same workers, which
    # keep what they hold: the cold build of a server whose workers are warm.
    . "$T/params"
    . "$work/env.sh"
    pkill -TERM -f "rargod --state $T/" 2>/dev/null || true
    wait_servers_gone
    pkill -KILL -f "rargod --state $T/" 2>/dev/null || true
    umount -l "$T/srv/mnt" 2>/dev/null || true
    rm -rf "$T/srv"
    i=1
    while [ "$i" -le "$n" ]; do umount -l "$T/w$i/mnt" 2>/dev/null || true; i=$((i + 1)); done
    start_server
    start_workers
    wait_up
    cat "$T/srv.log"
    ;;
down)
    down
    ;;
*)
    echo "usage: $0 up SERVER_JOBS WORKER_JOBS [N_WORKERS] [DELAY_MS] [LOSS_PCT] [RATE] | reset-server | down"; exit 2 ;;
esac
