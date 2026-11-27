#!/bin/sh
# forks/build.sh: build the planner (the cargo fork forks/setup.sh cargo made)
# against crates/rargo-plan of this checkout, and print its path. Run it again
# whenever crates/rargo-plan changes.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
work=${RARGO_WORK:-/root/rargo-work}
plan=$(cd "$here/../crates/rargo-plan" && pwd)
[ ! -f "$work/env.sh" ] || . "$work/env.sh"
[ -d "$work/cargo/.git" ] || { echo "no planner checkout at $work/cargo: run forks/setup.sh cargo first" >&2; exit 1; }
cd "$work/cargo"
# forks/cargo/0001 names rargo-plan at /root/rargo; point it at this checkout.
sed -i "s|^rargo-plan = { path = \"[^\"]*\" }|rargo-plan = { path = \"$plan\" }|" Cargo.toml
grep -qxF "rargo-plan = { path = \"$plan\" }" Cargo.toml \
    || { echo "$work/cargo/Cargo.toml does not name rargo-plan as forks/cargo/0001 does" >&2; exit 1; }
command -v cargo >/dev/null || { echo "no cargo on PATH: install Rust (https://rustup.rs) to build the planner" >&2; exit 1; }
cap() { if command -v systemd-run >/dev/null && [ "$(id -u)" = 0 ]; then bin=$(command -v "$1"); shift; systemd-run --scope -q -p CPUQuota=2400% -p MemoryMax=90G "$bin" "$@"; else "$@"; fi; }
export CFG_RELEASE_CHANNEL=nightly
cap cargo build --release --bin cargo
echo "$work/cargo/target/release/cargo"
