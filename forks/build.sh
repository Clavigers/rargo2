#!/bin/sh
# Builds the forked cargo. Its base is the cargo of a nightly toolchain, the
# one rargo's projects build with, and it is built on the nightly channel so
# that unstable features are gated as that toolchain's cargo gates them.
set -eu
work=${RARGO_WORK:-/root/rargo-work}
. "$work/env.sh"
cd "$work/cargo"
CFG_RELEASE_CHANNEL=nightly systemd-run --scope -q -p CPUQuota=2400% -p MemoryMax=90G \
    cargo build --release --bin cargo
echo "$work/cargo/target/release/cargo"
