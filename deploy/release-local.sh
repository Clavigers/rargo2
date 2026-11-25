#!/bin/bash
set -euo pipefail
tag=${1:?a tag, vX.Y.Z}
v=${tag#v}
repo=$(cd "$(dirname "$0")/.." && pwd)
"$repo/dev/release-gate.sh" "$(git -C "$repo" rev-parse "$tag^{commit}")"
dl=${RARGO_SITE_DATA:-/root/rargo-hq/data}/dl
mkdir -p "$dl/$v.tmp"
(cd "$repo" && gh release download "$tag" --dir "$dl/$v.tmp" --clobber)
(cd "$dl/$v.tmp" && sha256sum -c SHA256SUMS >/dev/null)
rm -rf "$dl/$v"; mv "$dl/$v.tmp" "$dl/$v"
chmod 644 "$dl/$v"/*
ln -sfn "$v" "$dl/latest"
echo "$v" >"$dl/VERSION"
ls -la "$dl/$v"
"$repo/deploy/update-service.sh" "$tag"
