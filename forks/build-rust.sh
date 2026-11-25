#!/bin/sh
set -eu
here=$(cd "$(dirname "$0")" && pwd)
work=${RARGO_WORK:-/root/rargo-work}
. "$work/env.sh"
cd "$work/rust"
[ -f bootstrap.toml ] || cp "$here/rust/bootstrap.toml" bootstrap.toml
systemd-run --scope -q -p CPUQuota=1600% -p MemoryMax=60G \
    ./x.py build --stage 1 library rustdoc clippy
host=$(rustc -vV | sed -n 's/^host: //p')
sysroot="$work/rust/build/$host/stage1"
touch "$sysroot/lib/rustlib/rargo-fork"
rustup toolchain link rargo "$sysroot"
echo "toolchain 'rargo' -> $sysroot (use: RUSTUP_TOOLCHAIN=rargo rargo build, or rargo +rargo build)"
