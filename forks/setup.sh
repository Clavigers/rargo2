#!/bin/sh
# forks/setup.sh [cargo] [rust]: clone and patch the forks into $RARGO_WORK
# (default /root/rargo-work). With no argument, both. The planner needs only
# cargo; rust is a large fetch, for the rargo toolchain (forks/build-rust.sh).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
work=${RARGO_WORK:-/root/rargo-work}
am() { git -C "$1" -c user.name=rargo -c user.email=noreply@rargo.invalid am "$2"/*.patch; }
# An existing clone is left alone, but said to be stale unless it is BASE
# followed by exactly as many commits as there are patches.
current() {
    base=$(cat "$here/$1/BASE")
    want=$(ls "$here/$1"/*.patch | wc -l)
    if git -C "$work/$1" merge-base --is-ancestor "$base" HEAD 2>/dev/null \
        && [ "$(git -C "$work/$1" rev-list --count "$base..HEAD")" = "$want" ]; then
        echo "$work/$1 already exists; not touching it"
    else
        echo "$work/$1 already exists and is not forks/$1/BASE plus the $want patches in forks/$1;" \
            "not touching it (remove it and run forks/setup.sh $1 again to start over)" >&2
    fi
}

setup_cargo() {
    base=$(cat "$here/cargo/BASE")
    if [ ! -d "$work/cargo/.git" ]; then
        git clone https://github.com/rust-lang/cargo.git "$work/cargo"
        git -C "$work/cargo" checkout -b rargo "$base"
        am "$work/cargo" "$here/cargo"
    else
        current cargo
    fi
}

setup_rust() {
    base=$(cat "$here/rust/BASE")
    if [ ! -d "$work/rust/.git" ]; then
        git init -q "$work/rust"
        git -C "$work/rust" remote add origin https://github.com/rust-lang/rust.git
        git -C "$work/rust" fetch --depth 1 origin "$base"
        git -C "$work/rust" checkout -b rargo FETCH_HEAD
        am "$work/rust" "$here/rust"
        cp "$here/rust/bootstrap.toml" "$work/rust/bootstrap.toml"
    else
        current rust
    fi
}

[ $# -gt 0 ] || set -- cargo rust
for fork in "$@"; do
    case $fork in
    cargo) setup_cargo ;;
    rust) setup_rust ;;
    *) echo "usage: $0 [cargo] [rust]" >&2; exit 2 ;;
    esac
done
