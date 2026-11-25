#!/bin/bash
# Builds the corpus for comparing chunking rules (#117): a project built by
# rargo at N successive first-parent commits, oldest first. After each
# build, one version is kept:
#   OUT/vNN/target/       everything the build leaves in target/debug
#   OUT/vNN/incremental/  the newest incremental state of every crate the
#                         server holds, as files (crates/rargo-core/
#                         examples/incrdump.rs)
#
#   dev/cdc-corpus.sh REPO TIP N OUT [rargo build arguments...]
#
# REPO is a git checkout of the project; it is not changed, the builds run
# in a worktree of it (OUT/src). The builds go to a private rargod of their
# own (state in OUT/server, port RARGO_CDC_PORT, default 7640), which keeps
# every crate's state between builds and is stopped for each dump. RARGO_BIN
# names the rargo and rargod to use, INCRDUMP the dump program.
set -euo pipefail
repo=$1 tip=$2 n=$3 out=$4
shift 4
here=$(cd "$(dirname "$0")" && pwd)
: "${RARGO_BIN:?the rargo and rargod to build with}" "${INCRDUMP:?the incrdump example}"
port=${RARGO_CDC_PORT:-7640}
export RARGO_STATE=$out/server RARGO_SERVER=127.0.0.1:$port RARGO_CACHE_DIR=$out/client-cache
export RARGO_JOBS=${RARGO_JOBS:-16} RARGO_CPU_QUOTA=${RARGO_CPU_QUOTA:-1600%}
mkdir -p "$out"
src=$out/src
git -C "$repo" rev-list --first-parent -n "$n" "$tip" | tac >"$out/commits"
[ -d "$src" ] || git -C "$repo" worktree add -q --detach "$src" "$(head -1 "$out/commits")"
trap '"$here/rargod.sh" stop >/dev/null 2>&1 || true' EXIT
i=0
while read -r c; do
    v=$(printf '%s/v%02d' "$out" "$i")
    if [ ! -d "$v" ]; then
        "$here/rargod.sh" start >/dev/null
        git -C "$src" checkout -q --detach "$c"
        (cd "$src" && "$RARGO_BIN/rargo" build "$@") >"$v.log" 2>&1 || { echo "build of $c failed: $v.log" >&2; exit 1; }
        rm -rf "$v.tmp"; mkdir -p "$v.tmp/target"
        cp -a "$src/target/debug/." "$v.tmp/target/"
        "$here/rargod.sh" stop >/dev/null
        "$INCRDUMP" "$RARGO_STATE" "$v.tmp/incremental" >"$v.dump"
        mv "$v.tmp" "$v"
        echo "$(basename "$v") $c target $(du -sb "$v/target" | cut -f1) B, incremental $(cat "$v.dump")"
    fi
    i=$((i + 1))
done <"$out/commits"
