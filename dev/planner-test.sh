#!/bin/bash
# deploy/planner.sh on a small stand-in for cargo, offline: BASE plus three
# patches, the first of which adds the rargo-plan path dependency and a
# Cargo.lock with an older rargo-plan than the release's. A release that adds
# a patch gets it, and the build leaves the planner checkout clean.
set -uo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
work=${RARGO_WORK:-/root/rargo-work}
[ -f "$work/env.sh" ] && . "$work/env.sh"
export CARGO_NET_OFFLINE=true
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
fail=0
g() { git -C "$1" -c user.name=t -c user.email=t@t.invalid "${@:2}"; }

cargo_repo=$tmp/cargo
mkdir -p "$cargo_repo/src"
printf '[package]\nname = "cargo"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\n' >"$cargo_repo/Cargo.toml"
echo 'fn main() { println!("base"); }' >"$cargo_repo/src/main.rs"
g "$cargo_repo" init -q
g "$cargo_repo" add -A
g "$cargo_repo" commit -q -m base
echo 'rargo-plan = { path = "/root/rargo/crates/rargo-plan" }' >>"$cargo_repo/Cargo.toml"
printf 'version = 4\n\n[[package]]\nname = "cargo"\nversion = "0.1.0"\ndependencies = [\n "rargo-plan",\n]\n\n[[package]]\nname = "rargo-plan"\nversion = "0.6.5"\n' >"$cargo_repo/Cargo.lock"
echo 'fn main() { println!("one {}", rargo_plan::V); }' >"$cargo_repo/src/main.rs"
g "$cargo_repo" add -A
g "$cargo_repo" commit -q -m 'rargo: planning mode'
sed -i 's/one/two/' "$cargo_repo/src/main.rs"
g "$cargo_repo" commit -q -a -m 'rargo: two'
sed -i 's/two/three/' "$cargo_repo/src/main.rs"
g "$cargo_repo" commit -q -a -m 'Plan install without placing binaries'
patches=$tmp/patches
g "$cargo_repo" format-patch -q --no-signature -o "$patches" HEAD~3..HEAD
base=$(git -C "$cargo_repo" rev-parse --short=9 HEAD~3)
g "$cargo_repo" checkout -q --detach "$base"
sed -i 's/base/other/' "$cargo_repo/src/main.rs"
g "$cargo_repo" commit -q -a -m 'not rargo-plan'
g "$cargo_repo" format-patch -q --no-signature --start-number 9 -o "$patches" HEAD~1..HEAD
g "$cargo_repo" checkout -q --detach "$base"

src=$tmp/src
mkdir -p "$src/forks/cargo" "$src/crates/rargo-plan/src"
echo "$base" >"$src/forks/cargo/BASE"
printf '[package]\nname = "rargo-plan"\nversion = "0.7.10"\nedition = "2021"\n' >"$src/crates/rargo-plan/Cargo.toml"
echo 'pub const V: &str = "0.7.10";' >"$src/crates/rargo-plan/src/lib.rs"
release() { rm -f "$src/forks/cargo/"*.patch; for p in "$@"; do cp "$patches/$p"-*.patch "$src/forks/cargo/"; done; }

planner=$tmp/planner build=$tmp/build
run() { RARGO_CARGO_URL=$cargo_repo "$here/deploy/planner.sh" "$src" "$planner" "$build" >"$tmp/out" 2>"$tmp/err"; }
clean() { [ -z "$(git -C "$planner" status --porcelain)" ]; }
built() {
    local what=$1 says=$2 subject=$3
    if run && [ "$("$(cat "$tmp/out")")" = "$says 0.7.10" ] && clean && [ "$(git -C "$planner" log -1 --format=%s)" = "$subject" ]; then
        echo "ok   $what"
    else
        echo "FAIL $what: $(cat "$tmp/out" "$tmp/err")"; git -C "$planner" status --short 2>&1; fail=1
    fi
}

release 0001 0002
built "a first deploy clones cargo, applies the release's patches and builds against its rargo-plan, leaving the checkout clean" two 'rargo: two'
release 0001 0002 0003
built "a release that adds a patch to forks/cargo gets it in the planner" three 'Plan install without placing binaries'
built "deploying the same release again builds the same planner" three 'Plan install without placing binaries'
echo '// local' >>"$planner/src/main.rs"; echo x >"$planner/stray"
built "changes left in the planner checkout are discarded" three 'Plan install without placing binaries'
g "$planner" checkout -q --detach "$base"
g "$planner" am -q "$src/forks/cargo/0001-"*.patch "$src/forks/cargo/0003-"*.patch >/dev/null 2>&1
built "a git am left half done is aborted" three 'Plan install without placing binaries'

refused() {
    local what=$1 expect=$2
    if run; then echo "FAIL $what: accepted"; fail=1; return; fi
    if ! grep -q "$expect" "$tmp/err"; then echo "FAIL $what: $(cat "$tmp/err")"; fail=1; return; fi
    echo "ok   $what: $(grep '^deploy/planner.sh: ' "$tmp/err" | sed 's/^deploy.planner.sh: //' | head -n 1 | sed "s|$tmp/||g")"
}
release 0001 0003
refused 'patches that do not apply' 'do not apply to cargo'
release 0009
refused 'patches without the rargo-plan dependency' 'does not name rargo-plan'
release 0001 0002 0003
echo 0000000000 >"$src/forks/cargo/BASE"
refused 'a BASE cargo does not have' 'cargo has no commit 0000000000'
exit $fail
