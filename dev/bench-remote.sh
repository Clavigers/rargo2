#!/bin/bash
set -u
work=${RARGO_WORK:-/root/rargo-work}
here=$(cd "$(dirname "$0")" && pwd)
export RARGO_BIN=${RARGO_BIN:-$here/../target/release}
. "$work/env.sh"
repo=${1:-ra}; shift || true
links=("$@"); [ ${#links[@]} -eq 0 ] && links=(lan 1 1gbit wan 20 100mbit)
jobs=${RARGO_JOBS:-24}
out=$work/bench/remote.jsonl
base=$work/bench/$repo
C=$work/remote-client
case $repo in
ra) main=(build -p rust-analyzer); edit=crates/stdx/src/lib.rs ;;
ruff) main=(build -p ruff); edit=crates/ruff_text_size/src/lib.rs ;;
bevy) main=(build -p bevy_ecs); edit=crates/bevy_utils/src/lib.rs ;;
zed) main=(build); edit=crates/collections/src/collections.rs; export GIT_LFS_SKIP_SMUDGE=1 RIG_PROXY=1; rargo_env="HTTPS_PROXY=http://127.0.0.1:3128"; stock=$here/zed-cargo.sh ;;
*) echo "unknown repo $repo"; exit 2 ;;
esac
stock=${stock:-cargo}
secs() { local t0=$(date +%s.%N); "$@" >"$C/last.log" 2>&1; local rc=$?; printf '%.2f %s' "$(echo "$(date +%s.%N) - $t0" | bc)" "$rc"; }
wt() {
    git -C "$base" worktree remove --force "$C/$1" >/dev/null 2>&1
    grep -q " $C/$1/target " /proc/self/mountinfo || rm -rf "$C/$1"
    git -C "$base" worktree prune
    git -C "$base" worktree add -q --detach "$C/$1" HEAD
}
field() { nsenter --net=/run/netns/${RIG:-rargo}-cl "$RARGO_BIN/rargo" --rargo-status | sed -n "s/^$1 *//p"; }
mkdir -p "$C"

if [ "${REMOTE_SKIP_STOCK:-}" != 1 ]; then
    wt stock
    cd "$C/stock" || exit 2
    cap() { systemd-run --scope -q -p CPUQuota=$((jobs * 100))% -p MemoryMax=90G "$@"; }
    read c1 r1 < <(secs cap "$stock" check --offline -j"$jobs")
    read c2 r2 < <(secs cap "$stock" check --offline -j"$jobs")
    printf '\n#[allow(dead_code)]\npub fn rargo_remote_bench() -> u32 { 7 }\n' >>"$edit"
    read c3 r3 < <(secs cap "$stock" check --offline -j"$jobs")
    read c4 r4 < <(secs cap "$stock" "${main[@]}" --offline -j"$jobs")
    read c5 r5 < <(secs cap "$stock" "${main[@]}" --offline -j"$jobs")
    printf '{"repo":"%s","config":"stock cargo, local","cold_check_s":%s,"noop_check_s":%s,"edit_check_s":%s,"build_s":%s,"noop_build_s":%s,"rc":[%s,%s,%s,%s,%s],"target_mib":%s}\n' \
        "$repo" "$c1" "$c2" "$c3" "$c4" "$c5" "$r1" "$r2" "$r3" "$r4" "$r5" "$(du -sm target | cut -f1)" | tee -a "$out"
    cd /
    git -C "$base" worktree remove --force "$C/stock" >/dev/null 2>&1
fi

set -- "${links[@]}"
while [ $# -ge 3 ]; do
    label=$1; delay=$2; rate=$3; shift 3
    RARGO_HIDE="$C" CLIENT_DELAY_MS=$delay CLIENT_RATE=$rate "$here/netns.sh" up "$jobs" 0 0 >/dev/null 2>&1
    . "$work/${RIG_STATE:-nstest}/client.env"
    R() { nsenter --net=/run/netns/${RIG:-rargo}-cl env ${rargo_env:-} "$RARGO_BIN/rargo" "$@"; }
    export RARGO_CACHE_DIR=$C/cache-1
    rm -rf "$C/cache-1" "$C/cache-2"
    wt one
    cd "$C/one" || exit 2
    read c1 r1 < <(secs R check)
    sync1=$(field sync)
    read c2 r2 < <(secs R check)
    printf '\n#[allow(dead_code)]\npub fn rargo_remote_bench() -> u32 { 7 }\n' >>"$edit"
    read c3 r3 < <(secs R check)
    read c4 r4 < <(secs R "${main[@]}")
    read c5 r5 < <(secs R "${main[@]}")
    mib=$(du -sm target | cut -f1)
    wt two
    cd "$C/two" || exit 2
    printf '\n#[allow(dead_code)]\npub fn rargo_remote_bench() -> u32 { 7 }\n' >>"$edit"
    export RARGO_CACHE_DIR=$C/cache-2
    read c6 r6 < <(RARGO_SYNC_STATS=1 secs R "${main[@]}")
    { grep '^rargo: ' "$C/last.log"; grep "rargod: phase" "$work/${RIG_STATE:-nstest}/srv.log" | tail -5; } >"$work/bench/logs/remote-$repo-second-$label.phases" 2>/dev/null
    printf '{"repo":"%s","config":"rargo, %s (%s ms each way, %s)","cold_check_s":%s,"noop_check_s":%s,"edit_check_s":%s,"build_s":%s,"noop_build_s":%s,"second_machine_build_s":%s,"rc":[%s,%s,%s,%s,%s,%s],"target_mib":%s,"first_sync":"%s","sync":"%s"}\n' \
        "$repo" "$label" "$delay" "$rate" "$c1" "$c2" "$c3" "$c4" "$c5" "$c6" "$r1" "$r2" "$r3" "$r4" "$r5" "$r6" "$mib" "$sync1" "$(field sync)" | tee -a "$out"
    cd /
    for n in 1 2; do RARGO_CACHE_DIR=$C/cache-$n R --rargo-daemon-stop >/dev/null 2>&1; done
    "$here/netns.sh" down >/dev/null 2>&1
done
git -C "$base" worktree remove --force "$C/one" >/dev/null 2>&1
git -C "$base" worktree remove --force "$C/two" >/dev/null 2>&1
