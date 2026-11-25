#!/bin/bash
set -u
work=${RARGO_WORK:-/root/rargo-work}
here=$(cd "$(dirname "$0")" && pwd)
export RARGO_BIN=${RARGO_BIN:-$here/../target/release}
. "$work/env.sh"
. "$here/bench-lib.sh"
repo=${1:-ra}; shift || true
links=("$@"); [ ${#links[@]} -eq 0 ] && links=(lan 1 40ms 20 100ms 50)
label=${LABEL:-after}
out=$work/bench/latency.jsonl
base=$work/bench/$repo
C=$work/latency-client
case $repo in
ra) edit=crates/stdx/src/lib.rs ;;
ruff) edit=crates/ruff_text_size/src/lib.rs ;;
*) echo "unknown repo $repo"; exit 2 ;;
esac
export RARGO_HIDE=$C RARGO_CACHE_DIR=$C/cache
R() { "$RARGO_BIN/rargo" "$@"; }
t() { local t0=$(date +%s.%N); "$@" >"$C/last.log" 2>&1 || echo "FAILED: $*" >&2; echo "$(date +%s.%N) - $t0" | bc; }
median() { sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}'; }
set -- "${links[@]}"
while [ $# -ge 2 ]; do
    name=$1; delay=$2; shift 2
    bench_down
    rm -rf "$C"; mkdir -p "$C"
    bench_up 24 0 0 $((2 * delay)) || exit 1
    . "$T/client.env"
    git -C "$base" worktree prune
    git -C "$base" worktree add -q --detach "$C/wt" HEAD
    cd "$C/wt" || exit 2
    first=$(t R build)
    noop=$(for i in 1 2 3 4 5 6 7; do t R build; done | median)
    now=$(for i in 1 2 3 4 5 6 7; do echo "// rargo latency $name now $i" >>"$edit"; t R build; done | median)
    settled=$(for i in 1 2 3 4 5 6 7; do echo "// rargo latency $name settled $i" >>"$edit"; sleep 1; t R build; done | median)
    printf '{"label":"%s","repo":"%s","link":"%s","rtt_ms":%s,"first_build_s":%.1f,"noop_s":%.3f,"edit_s":%.3f,"edit_settled_s":%.3f}\n' \
        "$label" "$repo" "$name" $((2 * delay)) "$first" "$noop" "$now" "$settled" | tee -a "$out"
    cd /
    git -C "$base" worktree remove --force "$C/wt" >/dev/null 2>&1
done
bench_down
rm -rf "$C"
