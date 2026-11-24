#!/bin/sh
# Print the path to the released Linux client used to build this workspace.
set -eu
version=0.3.6
digest=546e16760a23b9bed4bbdcfb8c3709e4b7e5fa6207459b6ff2403f17e7c4554c
dir=${XDG_CACHE_HOME:-$HOME/.cache}/rargo-bootstrap/$version
client=$dir/rargo
verify() { printf '%s  %s\n' "$digest" "$1" | sha256sum --check --status; }
if [ ! -x "$client" ] || ! verify "$client"; then
    mkdir -p "$dir"
    tmp=$(mktemp "$dir/download.XXXXXX")
    trap 'rm -f "$tmp"' EXIT
    gh release download "v$version" --repo sadosystems/rargo2 \
        --pattern rargo-x86_64-unknown-linux-musl --output "$tmp" --clobber
    verify "$tmp" || { echo "rargo v$version checksum mismatch" >&2; exit 1; }
    chmod 755 "$tmp"
    mv -f "$tmp" "$client"
fi
printf '%s\n' "$client"
