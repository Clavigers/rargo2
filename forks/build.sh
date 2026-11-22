#!/bin/sh
# Builds the forked cargo. The stable channel is forced so that unstable
# features are gated exactly as in a released cargo; the fork switches itself
# to nightly behaviour when the active toolchain is a nightly.
set -eu
work=${RARGO_WORK:-/root/rargo-work}
. "$work/env.sh"
cd "$work/cargo"
CFG_RELEASE_CHANNEL=stable systemd-run --scope -q -p CPUQuota=2400% -p MemoryMax=90G \
    cargo build --release --bin cargo
echo "$work/cargo/target/release/cargo"
