#!/bin/sh
# Regenerates forks/cargo/*.patch from the working checkout.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
work=${RARGO_WORK:-/root/rargo-work}
base=$(cat "$here/cargo/BASE")
rm -f "$here"/cargo/*.patch
git -C "$work/cargo" format-patch -q --no-signature -o "$here/cargo" "$base"..HEAD
ls "$here/cargo"
