#!/bin/bash
# forks/setup.sh and forks/build.sh without the network: a stale clone is
# named stale, and the planner is built against this checkout's rargo-plan
# wherever the checkout is.
set -uo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
fail=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; [ -n "${2:-}" ] && sed 's/^/     | /' <<<"$2"; fail=1; }
g() { git -C "$1" -c user.name=t -c user.email=t@t.invalid "${@:2}"; }

out=$(RARGO_WORK=$tmp/w "$here/forks/setup.sh" nonsense 2>&1); rc=$?
[ $rc = 2 ] && grep -q 'usage:' <<<"$out" && ok "setup.sh refuses an unknown fork" || bad "setup.sh refuses an unknown fork" "$rc $out"

mkdir -p "$tmp/w/cargo"
g "$tmp/w/cargo" init -q && g "$tmp/w/cargo" commit -q --allow-empty -m other
out=$(RARGO_WORK=$tmp/w "$here/forks/setup.sh" cargo 2>&1); rc=$?
[ $rc = 0 ] && grep -q "is not forks/cargo/BASE plus the $(ls "$here"/forks/cargo/*.patch | wc -l) patches" <<<"$out" \
    && ok "setup.sh says a clone that is not BASE plus the patches is stale" || bad "setup.sh says a clone that is not BASE plus the patches is stale" "$rc $out"
[ ! -e "$tmp/w/rust" ] && ok "setup.sh cargo leaves rust alone" || bad "setup.sh cargo leaves rust alone"

out=$(RARGO_WORK=$tmp/none "$here/forks/build.sh" 2>&1); rc=$?
[ $rc != 0 ] && grep -q 'forks/setup.sh cargo' <<<"$out" && ok "build.sh without a clone says to run setup.sh" || bad "build.sh without a clone says to run setup.sh" "$rc $out"

grep -h '^+rargo-plan = ' "$here"/forks/cargo/0001-*.patch | cut -c2- >"$tmp/w/cargo/Cargo.toml"
mkdir -p "$tmp/bin"
printf '#!/bin/sh\necho "$CFG_RELEASE_CHANNEL $*" >"%s"\n' "$tmp/args" >"$tmp/bin/cargo"; chmod +x "$tmp/bin/cargo"
out=$(PATH=$tmp/bin:$PATH RARGO_WORK=$tmp/w "$here/forks/build.sh" 2>&1); rc=$?
[ $rc = 0 ] && [ "$(tail -n 1 <<<"$out")" = "$tmp/w/cargo/target/release/cargo" ] && ok "build.sh prints where the planner is" || bad "build.sh prints where the planner is" "$rc $out"
[ "$(cat "$tmp/w/cargo/Cargo.toml")" = "rargo-plan = { path = \"$here/crates/rargo-plan\" }" ] \
    && ok "the planner names this checkout's rargo-plan" || bad "the planner names this checkout's rargo-plan" "$(cat "$tmp/w/cargo/Cargo.toml")"
[ "$(cat "$tmp/args" 2>/dev/null)" = "nightly build --release --bin cargo" ] && ok "build.sh builds the planner" || bad "build.sh builds the planner" "$(cat "$tmp/args" 2>&1)"
exit $fail
