#!/bin/bash
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
refuse() {
    echo "deploy/update-service.sh: $1" >&2
    echo "The service runs released versions only: dev/release.sh X.Y.Z releases and deploys one; deploy/update-service.sh vX.Y.Z deploys a published release again." >&2
    exit 1
}
want=${1:-}
[[ "$want" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] || refuse "'$want' is not a release tag (vX.Y.Z)"
assets=$(gh release view "$want" --repo "${RARGO_RELEASE_REPO:-sadosystems/rargo2}" --json isDraft,assets \
    --jq 'select(.isDraft | not) | .assets[].name' 2>/dev/null) || refuse "$want has no published release"
grep -qx rargo-x86_64-unknown-linux-musl <<<"$assets" || refuse "the release $want does not have the client (rargo-x86_64-unknown-linux-musl)"
S=${RARGO_SERVICE:-/root/rargo-work/service}
work=${RARGO_WORK:-/root/rargo-work}
. "$work/env.sh"
if [ ! -d "$S/src/.git" ]; then
    git clone -q "$(git -C "$repo" remote get-url origin)" "$S/src"
fi
git -C "$S/src" fetch -q origin
git -C "$S/src" fetch -q origin "refs/tags/$want:refs/tags/$want"
commit=$(git -C "$S/src" rev-parse --short=7 "refs/tags/$want^{commit}")
[ "$(git -C "$S/src" show "$commit:Cargo.toml" | sed -n 's/^version = "\(.*\)"/\1/p' | head -n 1)" = "${want#v}" ] || refuse "Cargo.toml at $want is not at ${want#v}"
git -C "$S/src" checkout -q --detach "$commit"
[ -z "$(git -C "$S/src" status --porcelain)" ] || { echo "the checkout is not clean" >&2; exit 1; }
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$S/src/Cargo.toml" | head -n 1)
rel=$S/releases/$version-$commit
if [ ! -x "$rel/rargod" ]; then
    cd "$S/src"
    cap() { systemd-run --scope -q -p CPUQuota=600% -p MemoryMax=20G "$@"; }
    CARGO_TARGET_DIR=$S/build cap cargo build --release --locked -p rargod -p rargo-core
    CARGO_TARGET_DIR=$S/build CC_x86_64_unknown_linux_musl=musl-gcc CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc \
        cap cargo build --release --locked -p rargo --target x86_64-unknown-linux-musl
    mkdir -p "$rel.tmp"
    cp "$S/build/release/rargod" "$S/build/release/rargo-sandbox" "$rel.tmp/"
    cp "$S/build/x86_64-unknown-linux-musl/release/rargo" "$rel.tmp/"
    strip "$rel.tmp/rargo"
    planner=$work/cargo
    [ -z "$(git -C "$planner" status --porcelain)" ] || { echo "the planner checkout is not clean" >&2; exit 1; }
    n=$(ls "$S/src/forks/cargo/"*.patch | wc -l)
    [ "$(git -C "$planner" rev-parse --short=9 "HEAD~$n")" = "$(cat "$S/src/forks/cargo/BASE")" ] || { echo "the planner checkout is not BASE + $n patches" >&2; exit 1; }
    last=$(ls "$S/src/forks/cargo/"*.patch | tail -n 1)
    subject=$(sed -n 's/^Subject: \[PATCH[^]]*\] //p' "$last" | head -n 1)
    case "$(git -C "$planner" log -1 --format=%s)" in
    "$subject"*) ;;
    *) echo "the planner's last commit is not the last patch" >&2; exit 1 ;;
    esac
    cp "$planner/target/release/cargo" "$rel.tmp/planner-cargo"
    cp -R "$S/src/nix/default" "$rel.tmp/default-flake"
    mv "$rel.tmp" "$rel"
fi
"$rel/rargod" --version
"$rel/rargo" --rargo-version
if [ "$(readlink "$S/current" 2>/dev/null)" != "$rel" ]; then
    ln -sfn "$rel" "$S/current"
    systemctl restart rargod.service
    sleep 1
fi
install -m 755 "$rel/rargo" /usr/local/bin/rargo
systemctl is-active rargod.service
echo "running: $rel"
