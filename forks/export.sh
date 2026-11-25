#!/bin/sh
set -eu
here=$(cd "$(dirname "$0")" && pwd)
work=${RARGO_WORK:-/root/rargo-work}
for name in cargo rust; do
    [ -d "$work/$name/.git" ] || continue
    base=$(cat "$here/$name/BASE")
    rm -f "$here/$name"/*.patch
    git -C "$work/$name" format-patch -q --no-signature -o "$here/$name" "$base"..HEAD
    ls "$here/$name"
done
