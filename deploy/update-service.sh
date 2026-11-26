#!/bin/bash
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
refuse() {
    echo "deploy/update-service.sh: $1" >&2
    echo "The service runs released versions only: dev/release.sh X.Y.Z releases and deploys one; deploy/update-service.sh vX.Y.Z deploys a published release again." >&2
    exit 1
}
fail() {
    echo "deploy/update-service.sh: $1" >&2
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
    planner=$S/planner
    plan_src=$S/src/crates/rargo-plan
    base=$(cat "$S/src/forks/cargo/BASE")
    [ -d "$planner/.git" ] || git clone -q https://github.com/rust-lang/cargo.git "$planner" || fail "cannot clone cargo into $planner"
    git -C "$planner" cat-file -e "$base^{commit}" 2>/dev/null || git -C "$planner" fetch -q origin || fail "cannot fetch cargo into $planner"
    git -C "$planner" am --abort 2>/dev/null || true
    git -C "$planner" checkout -q -f --detach "$base" || fail "cargo has no commit $base (forks/cargo/BASE at $want)"
    git -C "$planner" clean -qfdx
    git -C "$planner" -c user.name=rargo -c user.email=noreply@rargo.invalid am -q "$S/src/forks/cargo/"*.patch \
        || fail "the patches in forks/cargo at $want do not apply to cargo $base"
    grep -qF 'rargo-plan = { path = "/root/rargo/crates/rargo-plan" }' "$planner/Cargo.toml" \
        || fail "the planner's Cargo.toml does not name rargo-plan as forks/cargo/0001 does, so it cannot be pointed at $plan_src"
    sed -i "s|/root/rargo/crates/rargo-plan|$plan_src|" "$planner/Cargo.toml"
    (cd "$planner" && CARGO_TARGET_DIR=$S/planner-build CFG_RELEASE_CHANNEL=stable cap cargo build --release --bin cargo) \
        || fail "cannot build the planner against $plan_src at $want"
    plan_version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$plan_src/Cargo.toml" | head -n 1)
    used=$(cd "$planner" && cargo tree --offline -e normal -i rargo-plan --depth 0) || fail "cannot read which rargo-plan the planner was built against"
    [ "$used" = "rargo-plan v$plan_version ($plan_src)" ] \
        || fail "the planner was built against '$used', not rargo-plan v$plan_version ($plan_src) at $want"
    cp "$S/planner-build/release/cargo" "$rel.tmp/planner-cargo"
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
