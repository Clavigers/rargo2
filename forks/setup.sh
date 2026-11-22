#!/bin/sh
# Clones upstream cargo at the pinned base and applies rargo's patches.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
work=${RARGO_WORK:-/root/rargo-work}
base=$(cat "$here/cargo/BASE")
if [ ! -d "$work/cargo/.git" ]; then
    git clone https://github.com/rust-lang/cargo.git "$work/cargo"
    git -C "$work/cargo" checkout -b rargo "$base"
    git -C "$work/cargo" -c user.name=rargo -c user.email=noreply@rargo.invalid am "$here"/cargo/*.patch
else
    echo "$work/cargo already exists; not touching it"
fi
