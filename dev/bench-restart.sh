#!/bin/bash
# What the first command of a new client daemon costs: a client in its own
# network namespace, a server with 24 cores one shaped link away.
#
#   [LABEL=after] dev/bench-restart.sh [REPO] [NAME DELAY_MS]...
#
# Default links: lan (1 ms each way) and 100ms (50 ms each way). For each,
# after a full build: the daemon stopped and the same build run (7 times),
# and the daemon stopped, a one-line edit that recompiles one crate and
# nothing above it, and the build run (7 times). Medians go to
# $RARGO_WORK/bench/restart.jsonl. A second passes between stopping a
# daemon and the next command, so that the old daemon is gone.
set -u
work=${RARGO_WORK:-/root/rargo-work}
here=$(cd "$(dirname "$0")" && pwd)
export RARGO_BIN=${RARGO_BIN:-$here/../target/release}
. "$work/env.sh"
repo=${1:-ra}; shift || true
links=("$@"); [ ${#links[@]} -eq 0 ] && links=(lan 1 100ms 50)
label=${LABEL:-after}
out=$work/bench/restart.jsonl
base=$work/bench/$repo
C=$work/restart-client
case $repo in
ra) edit=crates/stdx/src/lib.rs ;;
ruff) edit=crates/ruff_text_size/src/lib.rs ;;
*) echo "unknown repo $repo"; exit 2 ;;
esac
export RARGO_HIDE=$C RARGO_CACHE_DIR=$C/cache
R() { nsenter --net=/run/netns/${RIG:-rargo}-cl "$RARGO_BIN/rargo" "$@"; }
t() { local t0=$(date +%s.%N); "$@" >"$C/last.log" 2>&1 || echo "FAILED: $*" >&2; echo "$(date +%s.%N) - $t0" | bc; }
median() { sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}'; }
set -- "${links[@]}"
while [ $# -ge 2 ]; do
    name=$1; delay=$2; shift 2
    "$here/netns.sh" down >/dev/null 2>&1
    rm -rf "$C"; mkdir -p "$C"
    CLIENT_DELAY_MS=$delay "$here/netns.sh" up 24 0 0 >/dev/null 2>&1
    . "$work/${RIG_STATE:-nstest}/client.env"
    git -C "$base" worktree prune
    git -C "$base" worktree add -q --detach "$C/wt" HEAD
    cd "$C/wt" || exit 2
    first=$(t R build)
    R build >/dev/null 2>&1
    same=$(for i in 1 2 3 4 5 6 7; do R --rargo-daemon-stop >/dev/null 2>&1; sleep 1; t R build; done | median)
    edit_s=$(for i in 1 2 3 4 5 6 7; do R --rargo-daemon-stop >/dev/null 2>&1; sleep 1; echo "// rargo restart $name $i" >>"$edit"; t R build; done | median)
    printf '{"label":"%s","repo":"%s","link":"%s","rtt_ms":%s,"first_build_s":%.1f,"restart_same_s":%.3f,"restart_edit_s":%.3f}\n' \
        "$label" "$repo" "$name" $((2 * delay)) "$first" "$same" "$edit_s" | tee -a "$out"
    R --rargo-daemon-stop >/dev/null 2>&1
    cd /
    git -C "$base" worktree remove --force "$C/wt" >/dev/null 2>&1
done
"$here/netns.sh" down >/dev/null 2>&1
rm -rf "$C"
