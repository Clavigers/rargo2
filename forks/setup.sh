#!/bin/sh
set -eu
here=$(cd "$(dirname "$0")" && pwd)
work=${RARGO_WORK:-/root/rargo-work}
am() { git -C "$1" -c user.name=rargo -c user.email=noreply@rargo.invalid am "$2"/*.patch; }

base=$(cat "$here/cargo/BASE")
if [ ! -d "$work/cargo/.git" ]; then
    git clone https://github.com/rust-lang/cargo.git "$work/cargo"
    git -C "$work/cargo" checkout -b rargo "$base"
    am "$work/cargo" "$here/cargo"
else
    echo "$work/cargo already exists; not touching it"
fi

base=$(cat "$here/rust/BASE")
if [ ! -d "$work/rust/.git" ]; then
    git init -q "$work/rust"
    git -C "$work/rust" remote add origin https://github.com/rust-lang/rust.git
    git -C "$work/rust" fetch --depth 1 origin "$base"
    git -C "$work/rust" checkout -b rargo FETCH_HEAD
    am "$work/rust" "$here/rust"
    cp "$here/rust/bootstrap.toml" "$work/rust/bootstrap.toml"
else
    echo "$work/rust already exists; not touching it"
fi
