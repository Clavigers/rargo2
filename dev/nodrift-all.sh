#!/bin/bash
# The no-drift check (dev/nodrift.sh) for the three benchmark repositories,
# against a fresh build server that cannot see them.
#   dev/nodrift-all.sh [ra|ruff|bevy ...]
set -u
work=${RARGO_WORK:-/root/rargo-work}
here=$(cd "$(dirname "$0")" && pwd)
export RARGO_BIN=${RARGO_BIN:-$here/../target/release}
export RARGO_STATE=$work/state-nodrift
export RARGO_SERVER=127.0.0.1:7474
export RARGO_HIDE=$work/bench
export RARGO_CACHE_DIR=$work/nodrift-client-cache
repos=("$@"); [ ${#repos[@]} -eq 0 ] && repos=(ra ruff bevy)
"$here/rargod.sh" stop >/dev/null 2>&1
rm -rf "$RARGO_STATE" "$RARGO_CACHE_DIR"
"$here/rargod.sh" start --drift-interval-min 0 >/dev/null || exit 1
trap '"$here/rargod.sh" stop >/dev/null 2>&1' EXIT
fails=0
for r in "${repos[@]}"; do
    case $r in
    ra) tests="" ;;
    ruff) tests="-p ruff_linter -p ruff_python_parser -p ruff_text_size" ;;
    bevy) tests="-p bevy_ecs -p bevy_math -p bevy_color" ;;
    *) echo "unknown repository $r"; exit 2 ;;
    esac
    echo "== $r"
    "$here/nodrift.sh" "$r" "$tests" || fails=$((fails + 1))
done
exit $fails
