#!/bin/bash
set -u
work=${RARGO_WORK:-/root/rargo-work}
here=$(cd "$(dirname "$0")" && pwd)
export RARGO_BIN=${RARGO_BIN:-$work/dev-target/release}
repo=${1:-ra}; shift || true
args=("$@"); [ ${#args[@]} -eq 0 ] && args=(build)
out=$work/bench/worker.jsonl
R() { nsenter --net=/run/netns/${RIG:-rargo}-cl "$RARGO_BIN/rargo" "$@"; }
export RARGO_HIDE=$work/bench/worker-wt
export RARGO_CACHE_DIR=$work/bench/worker-client-cache
base=$work/bench/$repo
wt=$work/bench/worker-wt
edit_file=$(cd "$base" && git ls-files 'crates/*/src/lib.rs' | head -1)

secs() { local t0=$(date +%s.%N); "$@" >"$work/bench/worker-last.log" 2>&1; local rc=$?; echo "$(echo "$(date +%s.%N) - $t0" | bc) $rc"; }
fresh_wt() {
    umount -l "$wt/target" 2>/dev/null
    git -C "$base" worktree remove --force "$wt" >/dev/null 2>&1
    rm -rf "$wt"
    git -C "$base" worktree prune
    git -C "$base" worktree add -q --detach "$wt" HEAD
}
measure() {
    . "$work/${RIG_STATE:-nstest}/client.env"
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
