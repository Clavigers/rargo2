work=${RARGO_WORK:-/root/rargo-work}
B=${RARGO_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/release}
case ${BENCH_STATE:-benchsrv} in /*) T=$BENCH_STATE ;; *) T=$work/${BENCH_STATE:-benchsrv} ;; esac
bench_port=${BENCH_PORT:-7559}
bench_wport=$((bench_port + 1))

bench_pids() {
    for pid in $(pgrep -x rargod 2>/dev/null) $(pgrep -x rargo 2>/dev/null); do
        if tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null | grep -q -- "--state $T/"; then
            printf '%s\n' "$pid"
        elif tr '\0' '\n' <"/proc/$pid/environ" 2>/dev/null | grep -qx "RARGO_SERVER=127.0.0.1:$bench_port"; then
            printf '%s\n' "$pid"
        fi
    done
}

bench_gone() {
    for pid in "$@"; do timeout "${BENCH_GRACE:-30}" sh -c 'while [ -d "/proc/$1" ]; do sleep 0.1; done' sh "$pid" || true; done
}

bench_stop() {
    pids=$(bench_pids | tr '\n' ' ')
    [ -n "$pids" ] || return 0
    kill -TERM $pids 2>&1 | grep -v 'No such process' >&2 || true
    bench_gone $pids
    pids=$(bench_pids | tr '\n' ' ')
    [ -n "$pids" ] && { kill -KILL $pids 2>&1 | grep -v 'No such process' >&2 || true; }
    bench_gone $pids
    return 0
}

bench_port_free() {
    [ -z "$(ss -Hlntu "( sport = :$1 )" 2>/dev/null)" ]
}

bench_wait_line() {
    timeout 30 tail -n +1 -f --pid="$3" "$1" | awk -v pat="$2" 'index($0, pat) { found = 1; exit } END { exit found ? 0 : 1 }'
}

bench_start() {
    mkdir -p "$T/srv"
    [ -f "$T/srv/tokens.json" ] || cp "$T/tokens.json" "$T/srv/tokens.json"
    hide=""
    for d in ${RARGO_HIDE:-}; do mkdir -p "$d"; hide="$hide mount -t tmpfs tmpfs '$d' &&"; done
    nohup env -u RARGO_LINK_RTT_MS systemd-run --scope -q -p CPUQuota=$((bench_sj * 100))% -p MemoryMax=60G -p Delegate=yes \
        unshare -m --propagation private sh -c "$hide exec \"\$@\"" sh \
        "$B/rargod" --state "$T/srv" --planner "${RARGO_PLANNER:-$work/cargo/target/release/cargo}" \
        --listen "127.0.0.1:$bench_port" --cargo-home "$work/cargo-home" \
        --jobs "$bench_sj" --workers-listen "127.0.0.1:$bench_wport" --worker-key "$T/link.key" --drift-interval-min 0 ${RARGOD_ARGS:-} >"$T/srv.log" 2>&1 &
    srvpid=$!
    if ! bench_wait_line "$T/srv.log" "serving clients" "$srvpid"; then
        echo "rargod did not start" >&2
        tail -n 20 "$T/srv.log" >&2
        return 1
    fi
    i=1
    while [ "$i" -le "$bench_n" ]; do
        mkdir -p "$T/w$i"
        nohup env RARGO_LINK_RTT_MS="${WORKER_RTT_MS:-2}" systemd-run --scope -q -p CPUQuota=$((bench_wj * 100))% -p MemoryMax=30G -p Delegate=yes \
            "$B/rargod" --state "$T/w$i" --worker "127.0.0.1:$bench_wport" --worker-key "$T/link.key" --jobs "$bench_wj" >>"$T/w$i.log" 2>&1 &
        i=$((i + 1))
    done
    joined=0
    while [ "$joined" -lt "$bench_n" ]; do
        if ! timeout 30 sh -c 'until [ "$(grep -c joined "$1")" -gt "$2" ]; do sleep 0.2; done' sh "$T/srv.log" "$joined"; then
            echo "only $joined of $bench_n workers joined" >&2
            tail -n 20 "$T"/w*.log >&2
            return 1
        fi
        joined=$((joined + 1))
    done
}

# bench_up SERVER_JOBS WORKER_JOBS N_WORKERS RTT_MS: a fresh server, and N workers, on loopback.
# The client's link gets RTT_MS more round trip through RARGO_LINK_RTT_MS, which holds every
# datagram the client sends for that long; the workers' links get WORKER_RTT_MS (2 by default).
bench_up() {
    bench_sj=$1; bench_wj=$2; bench_n=$3
    bench_stop
    for p in "$bench_port" "$bench_wport"; do
        bench_port_free "$p" || { echo "port $p is in use; set BENCH_PORT" >&2; return 1; }
    done
    rm -rf "$T"; mkdir -p "$T/srv"
    . "$work/env.sh"
    head -c 32 /dev/urandom >"$T/link.key"
    token=$("$B/rargod" token create --state "$T/srv" --user bench --admin 2>/dev/null)
    cp "$T/srv/tokens.json" "$T/tokens.json"
    printf 'export RARGO_SERVER=127.0.0.1:%s RARGO_TOKEN=%s RARGO_STATE=%s RARGO_LINK_RTT_MS=%s\n' \
        "$bench_port" "$token" "$T/srv" "${4:-0}" >"$T/client.env"
    bench_start
}

bench_reset_server() {
    bench_stop
    for i in $(seq 1 "$bench_n"); do umount -l "$T/w$i/mnt" 2>/dev/null || true; done
    umount -l "$T/srv/mnt" 2>/dev/null || true
    rm -rf "$T/srv"
    bench_start
}

bench_down() {
    bench_stop
}

# The bytes the client's link has received, as the client daemon of RARGO_CACHE_DIR counts them.
rx() {
    "$B/rargo" --rargo-daemon-status 2>&1 | sed -n 's/.* bytes sent, \([0-9]*\) bytes received.*/\1/p' | head -1 | grep . || echo 0
}
