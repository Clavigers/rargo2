#!/bin/sh
# Builds rargo's rustc fork (compiler, standard library, rustdoc, clippy) and
# registers it with rustup as the toolchain `rargo`. The marker file is how
# the planner recognises the fork (see DECISIONS D22).
#
# The build is the distribution configuration minus profile-guided
# optimisation, so this compiler is slower than an official release of the
# same version (about 1.4x in CPU time on a `cargo check` of rust-analyzer).
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
