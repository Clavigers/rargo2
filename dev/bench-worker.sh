#!/bin/bash
# The phase 4 measurement: does a worker shorten a cold build roughly in
# proportion to the cores it adds, and is a warm build no slower with one?
#
#   [CORES=8] dev/bench-worker.sh [REPO] [BUILD ARGS...]      (default: ra, build)
#
# Server and workers run in separate network namespaces (dev/netns.sh) with
# 1 ms delay each way, 0.1% loss and a 1 Gbit rate limit on every link. Each
# daemon is capped at its own number of cores. Configurations:
#   local-8      server with 8 cores, no worker
#   8+8 fresh    server 8 + one worker of 8 that has never seen anything
#   8+8 warm     the same after the server lost everything and the worker
#                kept its content cache (the toolchain is already there)
#   8+8+8 warm   two workers
#   local-16     server with 16 cores, no worker (what the cores are worth
#                without a network in between)
# For each: a cold build, the same build again, and a rebuild after an edit.
set -u
work=${RARGO_WORK:-/root/rargo-work}
here=$(cd "$(dirname "$0")" && pwd)
export RARGO_BIN=${RARGO_BIN:-$work/dev-target/release}
repo=${1:-ra}; shift || true
args=("$@"); [ ${#args[@]} -eq 0 ] && args=(build)
out=$work/bench/worker.jsonl
# The client runs in its own namespace, one shaped link from the server.
R() { nsenter --net=/run/netns/rargo-cl "$RARGO_BIN/rargo" "$@"; }
export RARGO_HIDE=$work/bench/worker-wt
export RARGO_CACHE_DIR=$work/bench/worker-client-cache
base=$work/bench/$repo
wt=$work/bench/worker-wt
edit_file=$(cd "$base" && git ls-files 'crates/*/src/lib.rs' | head -1)

secs() { local t0=$(date +%s.%N); "$@" >"$work/bench/worker-last.log" 2>&1; local rc=$?; echo "$(echo "$(date +%s.%N) - $t0" | bc) $rc"; }
fresh_wt() {
    git -C "$base" worktree remove --force "$wt" >/dev/null 2>&1
    git -C "$base" worktree prune
    git -C "$base" worktree add -q --detach "$wt" HEAD
}
measure() { # label
    . "$work/nstest/client.env"
    rm -rf "$RARGO_CACHE_DIR"
    fresh_wt
    cd "$wt" || exit 2
    read cold rc1 < <(secs R "${args[@]}")
    read again rc2 < <(secs R "${args[@]}")
    printf '\n#[allow(dead_code)]\npub fn rargo_worker_bench() -> u32 { 7 }\n' >>"$edit_file"
    read edit rc3 < <(secs R "${args[@]}")
    local st; st=$(R --rargo-status)
    local workers; workers=$(grep -A8 '^workers' <<<"$st" | tr '\n' ' ' | tr -s ' ')
    local incr; incr=$(grep '^incremental' <<<"$st" | tr -s ' ')
    printf '{"repo":"%s","config":"%s","cold_s":%.1f,"again_s":%.2f,"edit_s":%.1f,"rc":[%s,%s,%s],"workers":"%s","incremental":"%s"}\n' \
        "$repo" "$1" "$cold" "$again" "$edit" "$rc1" "$rc2" "$rc3" "$workers" "$incr" | tee -a "$out"
    cd /
}

c=${CORES:-8}
"$here/netns.sh" up $c $c 0 >/dev/null 2>&1;              measure "local-$c"
"$here/netns.sh" up $c $c 1 1 0.1 1gbit >/dev/null 2>&1;   measure "$c+$c fresh worker"
"$here/netns.sh" reset-server >/dev/null 2>&1;            measure "$c+$c warm worker"
"$here/netns.sh" up $c $c 2 1 0.1 1gbit >/dev/null 2>&1;   measure "$c+$c+$c fresh workers" >/dev/null
"$here/netns.sh" reset-server >/dev/null 2>&1;            measure "$c+$c+$c warm workers"
"$here/netns.sh" up $((2 * c)) $c 0 >/dev/null 2>&1;      measure "local-$((2 * c))"
"$here/netns.sh" up $((3 * c)) $c 0 >/dev/null 2>&1;      measure "local-$((3 * c))"
"$here/netns.sh" down >/dev/null 2>&1
git -C "$base" worktree remove --force "$wt" >/dev/null 2>&1
