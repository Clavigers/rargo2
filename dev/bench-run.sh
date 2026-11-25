#!/bin/bash
set -u
work=${RARGO_WORK:-/root/rargo-work}
here=$(cd "$(dirname "$0")" && pwd)
export RARGO_BIN=${RARGO_BIN:-$here/../target/release}
. "$work/env.sh"
label=${1:?label}; delay=${2:-0}
export RARGO_OUTPUT=fuse
out=$work/bench/run.jsonl
base=$work/bench/ra
export RIG=${RIG:-benchrun} RIG_STATE=${RIG_STATE:-benchrun}
C=$work/benchrun-client
export RARGO_HIDE=$C
R() { nsenter --net=/run/netns/$RIG-cl "$RARGO_BIN/rargo" "$@"; }
rx() { ip netns exec $RIG-cl cat /sys/class/net/eth0/statistics/rx_bytes; }
t() { local t0=$(date +%s.%N); "$@" >"$C/last.log" 2>&1 || { echo "FAILED: $*" >&2; tail -n 20 "$C/last.log" >&2; }; echo "$(date +%s.%N) - $t0" | bc; }
"$here/netns.sh" down >/dev/null 2>&1
rm -rf "$C"; mkdir -p "$C"
git -C "$base" worktree prune
CLIENT_DELAY_MS=$delay "$here/netns.sh" up 24 0 0 >/dev/null 2>&1
. "$work/$RIG_STATE/client.env"
wt() { git -C "$base" worktree add -q --detach "$C/$1" HEAD; }
wt a
(cd "$C/a" && RARGO_CACHE_DIR=$C/cache-a t R build -p rust-analyzer) >/dev/null
wt b
cd "$C/b" || exit 2
export RARGO_CACHE_DIR=$C/cache-b
build_s=$(t R build -p rust-analyzer)
b0=$(rx)
run_s=$(t R run -p rust-analyzer -- --version)
b1=$(rx)
again_s=$(t R run -p rust-analyzer -- --version)
b2=$(rx)
size=$(stat -c %s target/debug/rust-analyzer)
cd /
printf '{"label":"%s","rtt_ms":%s,"binary_bytes":%s,"build_s":%.2f,"run_s":%.2f,"run_link_down_bytes":%s,"run_again_s":%.2f,"run_again_link_down_bytes":%s}\n' \
    "$label" $((2 * delay)) "$size" "$build_s" "$run_s" $((b1 - b0)) "$again_s" $((b2 - b1)) | tee -a "$out"
for c in "$C/cache-a" "$C/cache-b"; do RARGO_CACHE_DIR=$c R --rargo-daemon-stop >/dev/null 2>&1; done
for w in a b; do git -C "$base" worktree remove --force "$C/$w" >/dev/null 2>&1; done
"$here/netns.sh" down >/dev/null 2>&1
rm -rf "$C"
