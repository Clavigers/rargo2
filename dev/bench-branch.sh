#!/bin/bash
set -u
work=${RARGO_WORK:-/root/rargo-work}
here=$(cd "$(dirname "$0")" && pwd)
export RARGO_BIN=${RARGO_BIN:-$here/../target/release}
. "$work/env.sh"
. "$here/bench-lib.sh"
mode=${1:?rargo or stock}; delay=${2:-1}
out=$work/bench/branch.jsonl
base=$work/bench/ra
C=$work/branch-client
edits=(crates/parser/src/lib.rs crates/syntax/src/lib.rs crates/ide-db/src/lib.rs)
tests=(test -p stdx -p parser -p syntax)
if [ "$mode" = rargo ]; then
    export RARGO_HIDE=$C RARGO_CACHE_DIR=$C/cache
    run() { "$RARGO_BIN/rargo" "$@"; }
    label="rargo, $((2 * delay)) ms round trip"
else
    run() { systemd-run --scope -q -p CPUQuota=2400% -p MemoryMax=90G cargo "$@" -j 24; }
    label="stock cargo"
fi
t() { local t0=$(date +%s.%N); "$@" >>"$C/last.log" 2>&1 || echo "FAILED: $*" >&2; echo "$(date +%s.%N) - $t0" | bc; }
all() {
    local c b s
    c=$(t run check); b=$(t run build -p rust-analyzer); s=$(t run "${tests[@]}")
    echo "$c $b $s"
}
bench_down
rm -rf "$C"; mkdir -p "$C"
git -C "$base" worktree prune
git -C "$base" branch -D rargo-bench-b >/dev/null 2>&1
[ "$mode" = rargo ] && { bench_up 24 0 0 $((2 * delay)) || exit 1; . "$T/client.env"; }
G() { git -c user.name=bench -c user.email=bench@example.invalid "$@"; }

git -C "$base" worktree add -q --detach "$C/c" HEAD
cd "$C/c" && read c0 b0 s0 < <(all)
git -C "$base" worktree add -q -b rargo-bench-b "$C/a" HEAD
cd "$C/a" && all >/dev/null
n=0
for f in "${edits[@]}"; do
    n=$((n + 1))
    printf '\npub fn rargo_branch_bench_%s() -> u32 { %s }\n' "$n" "$n" >>"$f"
    G commit -qam "branch work $n"
    all >/dev/null
done
sleep 2
read ca ba sa < <(all)
git -C "$base" worktree add -q --detach "$C/b" rargo-bench-b
cd "$C/b" && read cb bb sb < <(all)
cd "$C/c" && git checkout -q --detach rargo-bench-b && read cc bc sc < <(all)
status=""
[ "$mode" = rargo ] && status=$(run --rargo-status | grep -E '^(sync|cache|actions run)' | tr -s ' ' | tr '\n' ';')
printf '{"label":"%s","base_first":[%.2f,%.2f,%.2f],"a_same_agent":[%.2f,%.2f,%.2f],"b_fresh_worktree":[%.2f,%.2f,%.2f],"c_switched_worktree":[%.2f,%.2f,%.2f],"status":"%s"}\n' \
    "$label" "$c0" "$b0" "$s0" "$ca" "$ba" "$sa" "$cb" "$bb" "$sb" "$cc" "$bc" "$sc" "$status" | tee -a "$out"
cd /
[ "$mode" = rargo ] && run --rargo-daemon-stop
for w in a b c; do git -C "$base" worktree remove --force "$C/$w" >/dev/null 2>&1; done
git -C "$base" branch -D rargo-bench-b >/dev/null 2>&1
bench_down
rm -rf "$C"
