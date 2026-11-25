#!/bin/sh
set -eu
work=${RARGO_WORK:-/root/rargo-work}
. "$work/env.sh"
cd "$work/cargo"
CFG_RELEASE_CHANNEL=nightly systemd-run --scope -q -p CPUQuota=2400% -p MemoryMax=90G \
    cargo build --release --bin cargo
echo "$work/cargo/target/release/cargo"
