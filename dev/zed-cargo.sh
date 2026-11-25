#!/bin/bash
work=${RARGO_WORK:-/root/rargo-work}
envf=$work/bench/zed-devenv.sh
if [ ! -s "$envf" ]; then
    (cd "$work/bench/zed" && GIT_LFS_SKIP_SMUDGE=1 nix --extra-experimental-features nix-command --extra-experimental-features flakes \
        --accept-flake-config print-dev-env >"$envf.tmp") && mv "$envf.tmp" "$envf" || exit 1
fi
keep_home=$CARGO_HOME
here=$PWD
. "$envf" >/dev/null 2>&1
cd "$here" || exit 1
export CARGO_HOME=$keep_home
exec "$(dirname "$(command -v rustc)")/cargo" "$@"
