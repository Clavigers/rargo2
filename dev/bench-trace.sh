#!/bin/bash
# What does running every compiler in a traced sandbox cost?
#
#   dev/bench-trace.sh [REPO] [COMMAND...]        (default: ra, check)
#
# The same cold command twice, both capped at 24 cores: stock cargo on the
# files directly, and rargo (fresh server and store, client on this machine
# so that no link is involved). For rargo the CPU time is split by where it
# was spent:
#   actions, user     the compilers computing (stock cargo's user time is the
#                     reference)
#   actions, system   the compilers in the kernel; what exceeds stock cargo's
#                     system time is the kernel's side of the sandbox file
#                     system (FUSE round trips, namespace setup)
#   server, fuse      rargod's threads that answer the sandbox file system:
#                     the server's side of tracing
#   server, other     everything else in rargod: planning, hashing and
#                     storing outputs, scheduling, talking to the client
# One JSON line per run goes to $RARGO_WORK/bench/trace.jsonl.
set -u
work=${RARGO_WORK:-/root/rargo-work}
repo=$(cd "$(dirname "$0")/.." && pwd)
. "$work/env.sh"
name=${1:-ra}; shift || true
args=("$@"); [ ${#args[@]} -eq 0 ] && args=(check)
base=$work/bench/$name
wt=$work/bench/trace-wt
out=$work/bench/trace.jsonl
jobs=${RARGO_JOBS:-24}
tick=$(getconf CLK_TCK)

fresh_wt() {
    git -C "$base" worktree remove --force "$wt" >/dev/null 2>&1
    git -C "$base" worktree prune
    rm -rf "$wt"
    git -C "$base" worktree add -q --detach "$wt" HEAD
}

# --- stock cargo -----------------------------------------------------------
fresh_wt
cd "$wt" || exit 2
cargo fetch -q >/dev/null 2>&1
t0=$(date +%s.%N)
systemd-run --scope -q -p CPUQuota=$((jobs * 100))% -p MemoryMax=90G \
    /usr/bin/time -f '%U %S' -o "$work/bench/trace-stock.time" cargo "${args[@]}" --offline >"$work/bench/trace-stock.log" 2>&1
stock_rc=$?
stock_wall=$(echo "$(date +%s.%N) - $t0" | bc)
read stock_user stock_sys <"$work/bench/trace-stock.time"
cd /

# --- rargo -----------------------------------------------------------------
export RARGO_STATE=$work/state-trace
export RARGO_BIN=${RARGO_BIN:-$repo/target/release}
port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
export RARGO_SERVER=127.0.0.1:$port
export RARGO_HIDE=$wt
export RARGO_CACHE_DIR=$work/bench/trace-client-cache
export RARGO_JOBS=$jobs
"$repo/dev/rargod.sh" stop >/dev/null 2>&1
rm -rf "$RARGO_STATE" "$RARGO_CACHE_DIR"
fresh_wt
"$repo/dev/rargod.sh" start >/dev/null || { cat "$RARGO_STATE/rargod.log"; exit 1; }
pid=$(pgrep -f "rargod --state $RARGO_STATE " | head -1)
# The scope the server and its actions run in.
cg=/sys/fs/cgroup$(sed -n 's/^0:://p' "/proc/$pid/cgroup")
while [ "${cg%.scope}" = "$cg" ] && [ "$cg" != /sys/fs/cgroup ]; do cg=$(dirname "$cg"); done
cd "$wt" || exit 2
t0=$(date +%s.%N)
"$RARGO_BIN/rargo" "${args[@]}" >"$work/bench/trace-rargo.log" 2>&1
rargo_rc=$?
rargo_wall=$(echo "$(date +%s.%N) - $t0" | bc)
cd /
# Per-thread CPU of the server, by thread name.
threads=$(for t in /proc/$pid/task/*; do
    comm=$(cat "$t/comm" 2>/dev/null) || continue
    # Fields 14 and 15 of stat, counted after the closing parenthesis.
    read -r ut st < <(sed 's/^.*) //' "$t/stat" 2>/dev/null | awk '{print $12, $13}')
    echo "$comm ${ut:-0} ${st:-0}"
done)
fuse=$(awk -v t="$tick" '$1 ~ /^fuse-/ {s += $2 + $3} END {printf "%.2f", s / t}' <<<"$threads")
# The whole process, threads that have exited included.
read -r put pst < <(sed 's/^.*) //' "/proc/$pid/stat" | awk '{print $12, $13}')
server_user=$(awk -v t="$tick" -v v="$put" 'BEGIN {printf "%.2f", v / t}')
server_sys=$(awk -v t="$tick" -v v="$pst" 'BEGIN {printf "%.2f", v / t}')
server=$(awk -v a="$server_user" -v b="$server_sys" 'BEGIN {printf "%.2f", a + b}')
all_user=$(awk '/^user_usec/ {printf "%.2f", $2 / 1e6}' "$cg/cpu.stat")
all_sys=$(awk '/^system_usec/ {printf "%.2f", $2 / 1e6}' "$cg/cpu.stat")
status=$("$RARGO_BIN/rargo" --rargo-status | grep -E '^(actions run|cache)' | tr -s ' ' | tr '\n' ';')
"$repo/dev/rargod.sh" stop >/dev/null 2>&1
git -C "$base" worktree remove --force "$wt" >/dev/null 2>&1
rm -rf "$RARGO_STATE" "$RARGO_CACHE_DIR"

python3 - "$out" <<PY
import json, sys
stock = dict(wall=$stock_wall, user=$stock_user, sys=$stock_sys, rc=$stock_rc)
server = dict(total=$server, user=$server_user, sys=$server_sys, fuse=$fuse)
actions = dict(user=round($all_user - $server_user, 2), sys=round($all_sys - $server_sys, 2))
rargo = dict(wall=$rargo_wall, rc=$rargo_rc, cgroup_user=$all_user, cgroup_sys=$all_sys, actions=actions, server=server)
stock_cpu = stock["user"] + stock["sys"]
kernel = actions["sys"] - stock["sys"]
row = dict(
    repo="$name", command="${args[*]}", jobs=$jobs, stock=stock, rargo=rargo,
    tracing=dict(
        kernel_side_s=round(kernel, 2),
        server_side_s=server["fuse"],
        share_of_stock_cpu_pct=round(100 * (kernel + server["fuse"]) / stock_cpu, 1),
        server_other_s=round(server["total"] - server["fuse"], 2),
        compilers_user_delta_s=round(actions["user"] - stock["user"], 2),
        wall_delta_s=round(rargo["wall"] - stock["wall"], 2),
    ),
    status="$status",
)
open(sys.argv[1], "a").write(json.dumps(row) + "\n")
print(json.dumps(row, indent=1))
PY
