#!/bin/bash
# The part of a release that happens on this machine, after the release
# workflow has published vX.Y.Z on GitHub:
#   - the release's files are put where rargo.rs serves them
#     (https://rargo.rs/dl/X.Y.Z/ and /dl/latest/, install.sh,
#     install-server.sh), so the installers and the site never name a
#     version that is not released;
#   - the rargod service on this machine is updated to the tag.
#
#   deploy/release-local.sh vX.Y.Z
set -euo pipefail
tag=${1:?a tag, vX.Y.Z}
v=${tag#v}
repo=$(cd "$(dirname "$0")/.." && pwd)
"$repo/dev/release-gate.sh" "$(git -C "$repo" rev-parse "$tag^{commit}")"
dl=${RARGO_SITE_DATA:-/root/rargo-hq/data}/dl
mkdir -p "$dl/$v.tmp"
(cd "$repo" && gh release download "$tag" --dir "$dl/$v.tmp" --clobber)
(cd "$dl/$v.tmp" && sha256sum -c SHA256SUMS >/dev/null)
"$repo/deploy/render-site-doc.sh" "$dl/$v.tmp/protocol.md" "$dl/$v.tmp/protocol.html"
rm -rf "$dl/$v"; mv "$dl/$v.tmp" "$dl/$v"
chmod 644 "$dl/$v"/*
ln -sfn "$v" "$dl/latest"
echo "$v" >"$dl/VERSION"
ls -la "$dl/$v"
"$repo/deploy/update-service.sh" "$tag"
