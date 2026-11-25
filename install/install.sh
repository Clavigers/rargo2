#!/bin/sh
set -eu
version=${1:-latest}
base=${RARGO_DL:-https://rargo.rs/dl}
case "$(uname -s)-$(uname -m)" in
Linux-x86_64) asset=rargo-x86_64-unknown-linux-musl ;;
*) echo "rargo has no prebuilt client for $(uname -s) $(uname -m); try: cargo install rargo" >&2; exit 1 ;;
esac
if [ -n "${RARGO_INSTALL_DIR:-}" ]; then dir=$RARGO_INSTALL_DIR
elif [ -d "$HOME/.cargo/bin" ]; then dir=$HOME/.cargo/bin
else dir=$HOME/.local/bin
fi
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
if [ "$version" = latest ]; then
    version=$(curl -fsSL "$base/VERSION") || { echo "cannot read $base/VERSION" >&2; exit 1; }
fi
echo "downloading $base/$version/$asset"
curl -fsSL -o "$tmp/$asset" "$base/$version/$asset"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/$version/SHA256SUMS"
(cd "$tmp" && grep " $asset\$" SHA256SUMS | sha256sum -c - >/dev/null) || { echo "checksum of $asset does not match" >&2; exit 1; }
mkdir -p "$dir"
install -m 755 "$tmp/$asset" "$dir/rargo"
echo "installed $("$dir/rargo" --rargo-version) at $dir/rargo"
case ":$PATH:" in
*":$dir:"*) ;;
*) echo "note: $dir is not on your PATH" ;;
esac
[ -e /dev/fuse ] || echo "note: no /dev/fuse here; results will be copied into target/ instead of served from it"
echo "next, in a cargo project: rargo @HOST (the server, for example rargo.rs), rargo auth, rargo build"
