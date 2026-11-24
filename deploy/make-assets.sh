#!/bin/sh
# Puts a release's downloadable files into a directory:
#   deploy/make-assets.sh OUT CLIENT RARGOD SANDBOX PLANNER
#     rargo-x86_64-unknown-linux-musl          the client (static)
#     rargo-server-x86_64-unknown-linux-gnu.tar.gz   rargod, rargo-sandbox, planner-cargo
#     install.sh, install-server.sh            the installers as of this commit
#     SHA256SUMS
set -eu
out=$1; here=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$out"
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
install -m 755 "$2" "$out/rargo-x86_64-unknown-linux-musl"
mkdir "$tmp/rargo-server"
install -m 755 "$3" "$tmp/rargo-server/rargod"
install -m 755 "$4" "$tmp/rargo-server/rargo-sandbox"
install -m 755 "$5" "$tmp/rargo-server/planner-cargo"
cp "$here/LICENSE" "$tmp/rargo-server/"
strip "$tmp/rargo-server/rargod" "$tmp/rargo-server/rargo-sandbox" "$tmp/rargo-server/planner-cargo" 2>/dev/null || true
tar -czf "$out/rargo-server-x86_64-unknown-linux-gnu.tar.gz" -C "$tmp" rargo-server
cp "$here/install/install.sh" "$here/install/install-server.sh" "$out/"
(cd "$out" && sha256sum rargo-x86_64-unknown-linux-musl rargo-server-x86_64-unknown-linux-gnu.tar.gz install.sh install-server.sh >SHA256SUMS)
ls -la "$out"
