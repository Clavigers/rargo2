#!/bin/bash
# Builds a commit of this repository from a clean checkout and makes it the
# running rargod service on this machine.
#
#   deploy/update-service.sh [COMMIT]        (default: origin's main)
#
# The service lives in $RARGO_SERVICE (default /root/rargo-work/service):
#   src/                 the clean checkout
#   releases/VERSION-COMMIT/   rargod, rargo-sandbox, rargo (same commit,
#                        static, the one clients install), planner-cargo
#   current -> releases/...    what rargod.service runs
#   state/ keys cargo-home/ rustup/   kept across updates
# Nothing is built from a working tree. The unit is restarted only when
# the release changed; builds in progress end with it and are simply run
# again by their clients.
set -euo pipefail
S=${RARGO_SERVICE:-/root/rargo-work/service}
work=${RARGO_WORK:-/root/rargo-work}
repo=$(cd "$(dirname "$0")/.." && pwd)
want=${1:-origin/main}
. "$work/env.sh"
if [ ! -d "$S/src/.git" ]; then
    git clone -q "$(git -C "$repo" remote get-url origin)" "$S/src"
fi
git -C "$S/src" fetch -q origin
commit=$(git -C "$S/src" rev-parse --short=7 "$want^{commit}")
git -C "$S/src" checkout -q --detach "$commit"
[ -z "$(git -C "$S/src" status --porcelain)" ] || { echo "the checkout is not clean" >&2; exit 1; }
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$S/src/Cargo.toml" | head -n 1)
rel=$S/releases/$version-$commit
if [ ! -x "$rel/rargod" ]; then
    cd "$S/src"
    cap() { systemd-run --scope -q -p CPUQuota=600% -p MemoryMax=20G "$@"; }
    CARGO_TARGET_DIR=$S/build cap cargo build --release --locked -p rargod -p rargo-core
    CARGO_TARGET_DIR=$S/build CC_x86_64_unknown_linux_musl=musl-gcc CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc \
        cap cargo build --release --locked -p rargo --target x86_64-unknown-linux-musl
    mkdir -p "$rel.tmp"
    cp "$S/build/release/rargod" "$S/build/release/rargo-sandbox" "$rel.tmp/"
    cp "$S/build/x86_64-unknown-linux-musl/release/rargo" "$rel.tmp/"
    strip "$rel.tmp/rargo"
    # The planner is the cargo fork at forks/cargo/BASE with forks/cargo's
    # patches, built by forks/build.sh; its checkout must be exactly that.
    planner=$work/cargo
    [ -z "$(git -C "$planner" status --porcelain)" ] || { echo "the planner checkout is not clean" >&2; exit 1; }
    n=$(ls "$S/src/forks/cargo/"*.patch | wc -l)
    [ "$(git -C "$planner" rev-parse --short=9 "HEAD~$n")" = "$(cat "$S/src/forks/cargo/BASE")" ] || { echo "the planner checkout is not BASE + $n patches" >&2; exit 1; }
    last=$(ls "$S/src/forks/cargo/"*.patch | tail -n 1)
    [ "$(git -C "$planner" log -1 --format=%s)" = "$(sed -n 's/^Subject: \[PATCH[^]]*\] //p' "$last" | head -n 1)" ] || { echo "the planner's last commit is not the last patch" >&2; exit 1; }
    cp "$planner/target/release/cargo" "$rel.tmp/planner-cargo"
    mv "$rel.tmp" "$rel"
fi
"$rel/rargod" --version
"$rel/rargo" --rargo-version
if [ "$(readlink "$S/current" 2>/dev/null)" != "$rel" ]; then
    ln -sfn "$rel" "$S/current"
    systemctl restart rargod.service
    sleep 1
fi
systemctl is-active rargod.service
echo "running: $rel"
