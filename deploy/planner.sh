#!/bin/bash
# deploy/planner.sh SRC PLANNER BUILD
# Builds the planner (the cargo fork) for the release checked out in SRC:
# PLANNER, a checkout of cargo that belongs to the deploy, is set to
# SRC/forks/cargo/BASE plus every patch there (so a release that adds a patch
# gets it), built against SRC/crates/rargo-plan with its target in BUILD, and
# left clean. Prints the path of the built planner.
set -euo pipefail
src=$1 planner=$2 build=$3
fail() {
    echo "deploy/planner.sh: $1" >&2
    exit 1
}
plan_src=$src/crates/rargo-plan
base=$(cat "$src/forks/cargo/BASE")
[ -d "$planner/.git" ] || git clone -q "${RARGO_CARGO_URL:-https://github.com/rust-lang/cargo.git}" "$planner" || fail "cannot clone cargo into $planner"
git -C "$planner" cat-file -e "$base^{commit}" 2>/dev/null || git -C "$planner" fetch -q origin || fail "cannot fetch cargo into $planner"
# git am needs an identity even to abort (none may be configured), and a git am
# a stopped deploy left half done must be aborted, or the next one cannot start.
am() { git -C "$planner" -c user.name=rargo -c user.email=noreply@rargo.invalid am "$@"; }
[ ! -d "$(git -C "$planner" rev-parse --path-format=absolute --git-path rebase-apply)" ] || am --abort || fail "cannot abort the git am left in $planner"
git -C "$planner" checkout -q -f --detach "$base" || fail "cargo has no commit $base (forks/cargo/BASE)"
git -C "$planner" clean -qfdx
am -q "$src/forks/cargo/"*.patch \
    || fail "the patches in forks/cargo do not apply to cargo $base"
grep -qF 'rargo-plan = { path = "/root/rargo/crates/rargo-plan" }' "$planner/Cargo.toml" \
    || fail "the planner's Cargo.toml does not name rargo-plan as forks/cargo/0001 does, so it cannot be pointed at $plan_src"
sed -i "s|/root/rargo/crates/rargo-plan|$plan_src|" "$planner/Cargo.toml"
(cd "$planner" && CARGO_TARGET_DIR=$build CFG_RELEASE_CHANNEL=stable cargo build --release --bin cargo >&2) \
    || fail "cannot build the planner against $plan_src"
plan_version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$plan_src/Cargo.toml" | head -n 1)
used=$(cd "$planner" && cargo tree --offline -e normal -i rargo-plan --depth 0) || fail "cannot read which rargo-plan the planner was built against"
[ "$used" = "rargo-plan v$plan_version ($plan_src)" ] \
    || fail "the planner was built against '$used', not rargo-plan v$plan_version ($plan_src)"
# The build changed Cargo.toml (and Cargo.lock, for the new rargo-plan
# version): put them back, so the checkout is left as the patches made it.
git -C "$planner" checkout -q -f HEAD -- .
[ -z "$(git -C "$planner" status --porcelain)" ] || fail "the build left $planner changed"
echo "$build/release/cargo"
