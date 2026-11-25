#!/bin/bash
set -u
work=${RARGO_WORK:-/root/rargo-work}
here=$(cd "$(dirname "$0")" && pwd)
export RARGO_BIN=${RARGO_BIN:-$here/../target/release}
export RARGO_STATE=$work/state-nodrift
export RARGO_SERVER=127.0.0.1:7474
export RARGO_HIDE=$work/bench
export RARGO_CACHE_DIR=$work/nodrift-client-cache
repos=("$@"); [ ${#repos[@]} -eq 0 ] && repos=(ra ruff bevy zed)
"$here/rargod.sh" stop >/dev/null 2>&1
rm -rf "$RARGO_STATE" "$RARGO_CACHE_DIR"
export GIT_LFS_SKIP_SMUDGE=1
"$here/rargod.sh" start --drift-interval-min 0 >/dev/null || exit 1
trap '"$here/rargod.sh" stop >/dev/null 2>&1' EXIT
fails=0
for r in "${repos[@]}"; do
    case $r in
    ra) tests="" ;;
    ruff) tests="-p ruff_linter -p ruff_python_parser -p ruff_text_size" ;;
    bevy) tests="-p bevy_ecs -p bevy_math -p bevy_color" ;;
    zed) tests="-p util -p rope -p sum_tree"; export NODRIFT_CARGO="$here/zed-cargo.sh" ;;
    *) echo "unknown repository $r"; exit 2 ;;
    esac
    [ "$r" = zed ] || unset NODRIFT_CARGO
    echo "== $r${FRESH:+ (fresh worktree)}"
    if [ "${FRESH:-}" = 1 ]; then
        base=$work/bench/$r; wt=$work/bench/fresh-$r
        git -C "$base" worktree remove --force "$wt" >/dev/null 2>&1; git -C "$base" worktree prune
        git -C "$base" worktree add -q --detach "$wt" HEAD || exit 2
        [ -f "$wt/Cargo.lock" ] || cp "$base/Cargo.lock" "$wt/" 2>/dev/null
        NODRIFT_DIR=$wt NODRIFT_SKIP_SERVER=1 "$here/nodrift.sh" "$r" "$tests" || fails=$((fails + 1))
        "$RARGO_BIN/rargo" --rargo-daemon-stop
        git -C "$base" worktree remove --force "$wt" >/dev/null 2>&1
    else
        "$here/nodrift.sh" "$r" "$tests" || fails=$((fails + 1))
    fi
done
exit $fails
