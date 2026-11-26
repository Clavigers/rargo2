#!/bin/bash
# deploy/planner.sh on a small stand-in for the cargo fork: BASE plus three
# patches, the first of which adds the rargo-plan path dependency and a
# Cargo.lock that has an older rargo-plan version than the release's.
set -uo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
work=${RARGO_WORK:-/root/rargo-work}
[ -f "$work/env.sh" ] && . "$work/env.sh"
export CARGO_NET_OFFLINE=true
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
fail=0
g() { git -C "$1" -c user.name=t -c user.email=t@t.invalid "${@:2}"; }

fork=$tmp/fork
mkdir -p "$fork/src"
cat >"$fork/Cargo.toml" <<'EOF'
[package]
name = "cargo"
version = "0.1.0"
edition = "2021"

[dependencies]
EOF
echo 'fn main() { println!("base"); }' >"$fork/src/main.rs"
g "$fork" init -q
g "$fork" add -A
g "$fork" commit -q -m base
echo 'rargo-plan = { path = "/root/rargo/crates/rargo-plan" }' >>"$fork/Cargo.toml"
cat >"$fork/Cargo.lock" <<'EOF'
version = 4

[[package]]
name = "cargo"
version = "0.1.0"
dependencies = [
 "rargo-plan",
]

[[package]]
name = "rargo-plan"
version = "0.6.5"
EOF
echo 'fn main() { println!("one {}", rargo_plan::V); }' >"$fork/src/main.rs"
g "$fork" add -A
g "$fork" commit -q -m 'rargo: planning mode'
sed -i 's/one/two/' "$fork/src/main.rs"
g "$fork" commit -q -a -m 'rargo: a subject long enough that git format-patch folds it over two lines of the patch header'
sed -i 's/two/three/' "$fork/src/main.rs"
g "$fork" commit -q -a -m 'Plan install without placing binaries'

src=$tmp/src
mkdir -p "$src/forks/cargo" "$src/crates/rargo-plan/src"
g "$fork" format-patch -q --no-signature -o "$src/forks/cargo" HEAD~3..HEAD
g "$fork" rev-parse --short=9 HEAD~3 >"$src/forks/cargo/BASE"
grep -q '^ ' "$src/forks/cargo/0002-"*.patch || { echo "FAIL the second patch's subject is not folded, so the test checks less than it says"; fail=1; }
printf '[package]\nname = "rargo-plan"\nversion = "0.7.10"\nedition = "2021"\n' >"$src/crates/rargo-plan/Cargo.toml"
echo 'pub const V: &str = "0.7.10";' >"$src/crates/rargo-plan/src/lib.rs"

planner=$tmp/planner build=$tmp/build
fresh() { rm -rf "$planner"; git clone -q "$fork" "$planner"; g "$planner" reset -q --hard "HEAD~$1"; }
run() { "$here/deploy/planner.sh" "$src" "$planner" "$build" >"$tmp/out" 2>"$tmp/err"; }
clean() { [ -z "$(git -C "$planner" status --porcelain)" ]; }

fresh 1
if run && [ "$("$(cat "$tmp/out")")" = "three 0.7.10" ] && clean &&
    [ "$(git -C "$planner" log -1 --format=%s)" = 'Plan install without placing binaries' ]; then
    echo "ok   a checkout one patch behind gets the release's new patch, and the planner is built against the release's rargo-plan"
else
    echo "FAIL a checkout one patch behind: $(cat "$tmp/out" "$tmp/err")"; git -C "$planner" status --short; fail=1
fi
if clean; then echo "ok   building leaves the checkout clean (its Cargo.lock has rargo-plan 0.6.5)"; else echo "FAIL building left the checkout changed"; fail=1; fi
head=$(git -C "$planner" rev-parse HEAD)
if run && [ "$("$(cat "$tmp/out")")" = "three 0.7.10" ] && clean && [ "$(git -C "$planner" rev-parse HEAD)" = "$head" ]; then
    echo "ok   deploying again with every patch in place applies nothing and builds again"
else
    echo "FAIL deploying again: $(cat "$tmp/out" "$tmp/err")"; fail=1
fi
fresh 3
if run && [ "$("$(cat "$tmp/out")")" = "three 0.7.10" ] && clean; then
    echo "ok   a checkout at BASE gets all three patches"
else
    echo "FAIL a checkout at BASE: $(cat "$tmp/out" "$tmp/err")"; fail=1
fi

refused() {
    local what=$1 expect=$2
    local before; before=$(git -C "$planner" rev-parse HEAD)
    if run; then echo "FAIL $what: accepted"; fail=1; return; fi
    if ! grep -q "$expect" "$tmp/err" || [ "$(wc -l <"$tmp/err")" -lt 2 ]; then echo "FAIL $what: $(cat "$tmp/err")"; fail=1; return; fi
    if [ "$(git -C "$planner" rev-parse HEAD)" != "$before" ]; then echo "FAIL $what: the checkout moved"; fail=1; return; fi
    echo "ok   $what: $(grep '^deploy/planner.sh: ' "$tmp/err" | sed 's/^deploy.planner.sh: //' | head -n 1 | sed "s|$tmp/||")"
}
fresh 1; echo '// local' >>"$planner/src/main.rs"
refused 'local changes' 'is not clean'
fresh 0; echo 'x' >"$planner/x"; g "$planner" add x; g "$planner" commit -q -m 'not a patch'
refused 'a commit that is not a patch' 'more than the 3 patches'
fresh 2; echo 'x' >"$planner/x"; g "$planner" add x; g "$planner" commit -q -m 'something else'
refused 'another commit where a patch belongs' 'where forks/cargo has 0002-'
fresh 1; sed -i 's/two/TWO/' "$planner/src/main.rs"; g "$planner" commit -q -a --amend --no-edit
refused 'a checkout the next patch does not apply to' 'does not take 0003-'
if clean && [ ! -e "$planner/.git/rebase-apply" ]; then echo "ok   and the failed git am is aborted, the checkout clean"; else echo "FAIL the failed git am left the checkout in the middle of it"; fail=1; fi
rm -rf "$planner"; git init -q "$planner"; echo x >"$planner/x"; g "$planner" add x; g "$planner" commit -q -m other
refused 'a checkout without BASE' 'does not have the commit'
exit $fail
