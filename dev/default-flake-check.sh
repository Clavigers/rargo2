#!/bin/bash
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
fails=0
check() {
    local name=$1 file=${2:-} body=${3:-}
    local d
    d=$(mktemp -d)
    cp "$here/nix/default/flake.nix" "$here/nix/default/flake.lock" "$d/"
    [ -n "$file" ] && printf '%s\n' "$body" > "$d/$file"
    if nix --extra-experimental-features 'nix-command flakes' develop "path:$d" --command sh -c \
        'lib="$(rustc --print sysroot)/lib/rustlib/src/rust/library"; rustc -V; test -f "$lib/std/src/lib.rs" && test -f "$lib/core/src/lib.rs" && test -f "$lib/alloc/src/lib.rs"'; then
        echo "ok   $name: the toolchain has lib/rustlib/src/rust/library"
    else
        echo "FAIL $name: the toolchain has no lib/rustlib/src/rust/library"
        fails=$((fails + 1))
    fi
    rm -rf "$d"
}
check "no toolchain file"
check "a toolchain file that asks for rustfmt alone" rust-toolchain.toml $'[toolchain]\nchannel = "stable"\ncomponents = ["rustfmt"]'
check "a legacy toolchain file" rust-toolchain "1.91"
[ "$fails" -eq 0 ]
