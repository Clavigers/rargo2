#!/bin/bash
set -euo pipefail
tag=${1:?a tag, vX.Y.Z}
v=${tag#v}
repo=$(cd "$(dirname "$0")/.." && pwd)
"$repo/dev/ci-passed.sh" "$(git -C "$repo" rev-parse "$tag^{commit}")"
dl=${RARGO_SITE_DATA:-/root/rargo-hq/data}/dl
mkdir -p "$dl/$v.tmp"
(cd "$repo" && gh release download "$tag" --dir "$dl/$v.tmp" --clobber)
(cd "$dl/$v.tmp" && sha256sum -c SHA256SUMS >/dev/null)
rm -rf "$dl/$v"; mv "$dl/$v.tmp" "$dl/$v"
chmod 644 "$dl/$v"/*
ls -la "$dl/$v"
"$repo/deploy/update-service.sh" "$tag"
# install.sh installs $dl/VERSION, so it names a version only once the build server runs it:
# a client newer than the server is refused.
ln -sfn "$v" "$dl/latest"
echo "$v" >"$dl/VERSION.tmp" && mv "$dl/VERSION.tmp" "$dl/VERSION"
"$repo/deploy/update-site.sh"
