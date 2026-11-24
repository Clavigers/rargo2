#!/bin/sh
# Puts a release's downloadable files into a directory:
#   deploy/make-assets.sh OUT CLIENT RARGOD SANDBOX PLANNER
#     rargo-x86_64-unknown-linux-musl          the client (static)
#     rargo-server-x86_64-unknown-linux-gnu.tar.gz   rargod, rargo-sandbox, planner-cargo
#     install.sh, install-server.sh            the installers as of this commit
#     rargo-default-flake.tar.gz              the default shell for stock Cargo
#     protocol.md                              the release protocol and schema
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
install -d "$tmp/rargo-server/default-flake"
install -m 644 "$here/nix/default/flake.nix" "$here/nix/default/flake.lock" "$tmp/rargo-server/default-flake/"
tar -czf "$out/rargo-default-flake.tar.gz" -C "$tmp/rargo-server" default-flake
cp "$here/LICENSE" "$tmp/rargo-server/"
strip "$tmp/rargo-server/rargod" "$tmp/rargo-server/rargo-sandbox" "$tmp/rargo-server/planner-cargo" 2>/dev/null || true
tar -czf "$out/rargo-server-x86_64-unknown-linux-gnu.tar.gz" -C "$tmp" rargo-server
cp "$here/install/install.sh" "$here/install/install-server.sh" "$out/"
version=$(sed -n 's/^version = "\([^"]*\)"$/\1/p' "$here/Cargo.toml" | head -n 1)
proto="$here/crates/rargo-proto/src/lib.rs"
number=$(sed -n 's/^pub const PROTO_VERSION: u32 = \([0-9][0-9]*\);$/\1/p' "$proto")
[ -n "$version" ] && [ -n "$number" ]
{
    printf '%s\n' '---' 'title: Protocol' "subtitle: Rargo $version · Protocol $number" '---' ''
    cat "$here/docs/protocol.md"
    printf '\n# Wire types\n\n'
    printf '%s\n' 'The declarations below give the serialized field and enum variant order.'
    printf '\n```rust\n'
    printf '%s\n\n' 'pub type Digest = [u8; 32];'
    for name in Frame Offer RunRequest ExecRequest Delivery Write ServerMsg FutureReport Blob ClientMsg Part; do
        sed -n -E "/^pub (struct|enum) $name[<{ ]/,/^}/p" "$proto"
        printf '\n'
    done
    for name in Node Tree; do
        sed -n -E "/^pub (struct|enum) $name[<{ ]/,/^}/p" "$here/crates/rargo-proto/src/tree.rs"
        printf '\n'
    done
    printf '```\n'
} >"$out/protocol.md"
(cd "$out" && sha256sum rargo-x86_64-unknown-linux-musl rargo-server-x86_64-unknown-linux-gnu.tar.gz rargo-default-flake.tar.gz install.sh install-server.sh protocol.md >SHA256SUMS)
ls -la "$out"
