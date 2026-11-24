#!/bin/bash
# Builds, runs and tests a small workspace through a rargo server, over UDP
# and over TCP: the check that a deployed service works for a client.
#
#   RARGO_SERVER=host:port RARGO_USER=name RARGO_KEY=secret deploy/check-service.sh [path/to/rargo]
#
# The workspace has a registry dependency and a rust-toolchain.toml, so it
# also checks that the server can download crates and install toolchains.
set -u
R=$(command -v "${1:-rargo}") || { echo "no rargo binary: $1" >&2; exit 2; }
: "${RARGO_SERVER:?}" "${RARGO_USER:?}" "${RARGO_KEY:?}"
T=$(mktemp -d "${TMPDIR:-/tmp}/rargo-check.XXXXXX")
export HOME=$T/home RARGO_CACHE_DIR=$T/cache
mkdir -p "$T/home" "$T/proj/src"
cd "$T/proj" || exit 2
git init -q .
cat >Cargo.toml <<'EOF'
[package]
name = "rargo-check"
version = "0.1.0"
edition = "2021"

[dependencies]
itoa = "1"
EOF
printf '[toolchain]\nchannel = "1.99.0"\n' >rust-toolchain.toml
cat >src/main.rs <<'EOF'
fn main() {
    let mut b = itoa::Buffer::new();
    println!("built remotely: {}", b.format(6 * 7));
}
#[test]
fn works() {
    assert_eq!(2 + 2, 4);
}
EOF
git add -A; git -c user.name=check -c user.email=check@rargo.invalid commit -qm one
fail=0
ok() { echo "ok    $1"; }
bad() { echo "FAIL  $1"; shift; printf '%s\n' "$@" | sed 's/^/        /'; fail=1; }
"$R" --rargo-version
for transport in udp tcp; do
    export RARGO_TRANSPORT=$transport
    # Each transport from nothing on the client: its own daemon and cache.
    export RARGO_CACHE_DIR=$T/cache-$transport
    echo "// $transport" >>src/main.rs
    out=$("$R" build 2>&1) && ok "$transport: build" || bad "$transport: build" "$out"
    out=$(./target/debug/rargo-check 2>&1); [ "$out" = "built remotely: 42" ] && ok "$transport: the binary runs here" || bad "$transport: the binary runs here" "$out"
    out=$("$R" test 2>&1); grep -q "test result: ok. 1 passed" <<<"$out" && ok "$transport: test" || bad "$transport: test" "$out"
    out=$("$R" --rargo-daemon-status 2>&1); grep -q "^link *$transport" <<<"$out" && ok "$transport: the link is $transport" || bad "$transport: the link is $transport" "$out"
    "$R" --rargo-daemon-stop >/dev/null 2>&1
done
cd /
rm -rf "$T"
exit $fail
