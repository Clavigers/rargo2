#!/bin/bash
set -u
work=${RARGO_WORK:-/root/rargo-work}
here=$(cd "$(dirname "$0")" && pwd)
export RARGO_BIN=${RARGO_BIN:-$here/../target/release}
. "$work/env.sh"
. "$here/bench-lib.sh"
mode=${1:?copy or fuse}; delay=${2:-50}
export RARGO_OUTPUT=$mode
out=$work/bench/fresh.jsonl
base=$work/bench/ra
C=$work/fresh-client
export RARGO_HIDE=$C
R() { "$RARGO_BIN/rargo" "$@"; }
t() { local t0=$(date +%s.%N); "$@" >>"$C/last.log" 2>&1 || echo "FAILED: $*" >&2; echo "$(date +%s.%N) - $t0" | bc; }
bench_down
rm -rf "$C"; mkdir -p "$C"
git -C "$base" worktree prune
bench_up 24 0 0 $((2 * delay)) || exit 1
. "$T/client.env"
fresh() {
    git -C "$base" worktree add -q --detach "$C/$1" HEAD
    cd "$C/$1" || exit 2
    export RARGO_CACHE_DIR=$2
    local b0 b r
    b0=$(rx)
    b=$(t R build -p rust-analyzer)
    r=$(t ./target/debug/rust-analyzer --version)
    echo "$b $r $(( $(rx) - b0 ))"
    cd /
}
read first_b first_r first_bytes < <(fresh a "$C/cache")
read same_b same_r same_bytes < <(fresh b "$C/cache")
read other_b other_r other_bytes < <(fresh c "$C/cache-other-machine")
size=$(stat -c %s "$C/a/target/debug/rust-analyzer" 2>/dev/null || echo 0)
printf '{"mode":"%s","rtt_ms":%s,"binary_bytes":%s,"first_worktree":{"build_s":%.2f,"run_s":%.2f,"link_down_bytes":%s},"fresh_worktree_same_machine":{"build_s":%.2f,"run_s":%.2f,"link_down_bytes":%s},"fresh_worktree_new_machine":{"build_s":%.2f,"run_s":%.2f,"link_down_bytes":%s}}\n' \
    "$mode" $((2 * delay)) "$size" "$first_b" "$first_r" "$first_bytes" "$same_b" "$same_r" "$same_bytes" "$other_b" "$other_r" "$other_bytes" | tee -a "$out"
for c in "$C/cache" "$C/cache-other-machine"; do RARGO_CACHE_DIR=$c R --rargo-daemon-stop; done
for w in a b c; do git -C "$base" worktree remove --force "$C/$w" >/dev/null 2>&1; done
bench_down
rm -rf "$C"
