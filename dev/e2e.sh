#!/bin/bash
# End-to-end checks against a private build server.
#
#   dev/e2e.sh            run everything
#
# The server and the client run on this machine but share nothing except a
# TCP port: the server runs in a mount namespace in which the client's
# directory (worktrees, cache, cargo home) is an empty file system, has its
# own cargo home, and requires a key.
#
# Uses private directories under $RARGO_WORK. Set RARGO_E2E_ROOT to an
# existing directory on a reflink filesystem to put the store and the client
# there. Stops its server when done. Requires root (FUSE mount, namespaces).
set -u
work=${RARGO_WORK:-/root/rargo-work}
repo=$(cd "$(dirname "$0")/.." && pwd)
. "$work/env.sh"
RARGO_STATE=$(mktemp -d "$work/e2e-state.XXXXXXXX") || exit 1
export RARGO_STATE
bin=${RARGO_BIN:-$repo/target/release}
export RARGO_BIN=$bin
R=$bin/rargo
E=$(mktemp -d "${RARGO_E2E_ROOT:-$work}/e2e.XXXXXXXX") || exit 1
E=$(cd "$E" && pwd -P) || exit 1
# Everything the client owns lives under $T; the server cannot see it.
T=$E/client
mkdir -p "$T" "$E/store"
port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
export RARGO_SERVER=127.0.0.1:$port
export RARGO_HIDE=$T
export RARGO_CACHE_DIR=$T/.rargo-cache
export RARGO_USER=e2e
export RARGO_KEY=e2e-client-secret-0123456789
printf 'e2e %s admin\nguest guest-secret-0123456789abcdef\n' "$RARGO_KEY" >"$RARGO_STATE/keys"
# The server gets a cargo home of its own.
export RARGO_SERVER_CARGO_HOME=$RARGO_STATE/cargo-home
# The stock Cargo checks intentionally exercise the fixture's own target/.
unset CARGO_TARGET_DIR
echo "e2e: state=$RARGO_STATE client=$T server=$RARGO_SERVER"
fails=0
checks=0

ok() { checks=$((checks + 1)); echo "  ok   $1"; }
bad() { checks=$((checks + 1)); fails=$((fails + 1)); echo "  FAIL $1"; [ -n "${2:-}" ] && echo "$2" | sed 's/^/       | /' | head -40; }
expect() { # description, command...
    local d=$1; shift
    if out=$("$@" 2>&1); then ok "$d"; else bad "$d" "$out"; fi
}
has() { grep -qF -- "$2" <<<"$1"; }
runs() { "$R" --rargo-status | awk '/^actions run/ {print $3}'; }
# Files and directory manifests the server has asked clients for so far.
asked_files() { "$R" --rargo-status | sed -n 's/^sync .* manifests and \([0-9]*\) files asked.*/\1/p'; }
asked_trees() { "$R" --rargo-status | sed -n 's/^sync .*states, \([0-9]*\) manifests.*/\1/p'; }

trap '"$repo/dev/rargod.sh" stop >/dev/null 2>&1' EXIT
"$repo/dev/rargod.sh" start --store "$E/store" --keys "$RARGO_STATE/keys" >/dev/null || {
    cat "$RARGO_STATE/rargod.log"
    exit 1
}

echo "== client and server are apart"
pid=$(pgrep -f "rargod --state $RARGO_STATE " | head -1)
echo marker >"$T/marker"
[ -n "$pid" ] && [ -d "/proc/$pid/root$T" ] && [ -z "$(ls -A "/proc/$pid/root$T" 2>/dev/null)" ] && ok "the server sees an empty directory where the client's files are" || bad "the server sees an empty directory where the client's files are" "$(ls -A "/proc/$pid/root$T" 2>&1)"
out=$(RARGO_KEY=wrong-secret-0123456789 "$R" --rargo-status 2>&1); rc=$?
[ $rc -ne 0 ] && has "$out" "key" && ok "a wrong key is refused" || bad "a wrong key is refused" "$out"
out=$(RARGO_USER=nobody "$R" --rargo-status 2>&1); rc=$?
[ $rc -ne 0 ] && ok "an unknown user is refused" || bad "an unknown user is refused" "$out"
out=$(RARGO_USER=guest RARGO_KEY=guest-secret-0123456789abcdef "$R" --rargo-gc 2>&1); rc=$?
[ $rc -ne 0 ] && has "$out" "may not administer" && ok "a user who is not an administrator cannot collect garbage" || bad "a user who is not an administrator cannot collect garbage" "$out"
expect "the right key is accepted" "$R" --rargo-status

# --- a small workspace -----------------------------------------------------
A=$T/a
mkdir -p "$A/crates/util/src" "$A/crates/app/src" "$A/crates/app/tests" "$A/crates/mac/src"
cat >"$A/Cargo.toml" <<'EOF'
[workspace]
resolver = "2"
members = ["crates/*"]
EOF
cat >"$A/crates/mac/Cargo.toml" <<'EOF'
[package]
name = "mac"
version = "0.1.0"
edition = "2021"
[lib]
proc-macro = true
EOF
cat >"$A/crates/mac/src/lib.rs" <<'EOF'
use proc_macro::TokenStream;
#[proc_macro]
pub fn answer(_: TokenStream) -> TokenStream {
    "42".parse().unwrap()
}
EOF
cat >"$A/crates/util/Cargo.toml" <<'EOF'
[package]
name = "util"
version = "0.1.0"
edition = "2021"
build = "build.rs"
[dependencies]
mac = { path = "../mac" }
EOF
cat >"$A/crates/util/build.rs" <<'EOF'
use std::{env, fs, path::PathBuf};
fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    fs::write(out.join("generated.rs"), "pub const GENERATED: &str = \"from build script\";\n").unwrap();
    println!("cargo:rustc-cfg=has_generated");
    println!("cargo:rustc-check-cfg=cfg(has_generated)");
    println!("cargo:rustc-env=UTIL_BUILD_NOTE=built-by-script");
    println!("cargo:rerun-if-env-changed=UTIL_FLAVOR");
    if let Ok(f) = env::var("UTIL_FLAVOR") {
        println!("cargo:rustc-env=UTIL_FLAVOR_BAKED={f}");
    }
    println!("cargo:warning=hello from the util build script");
}
EOF
cat >"$A/crates/util/src/lib.rs" <<'EOF'
#[cfg(has_generated)]
include!(concat!(env!("OUT_DIR"), "/generated.rs"));
pub fn note() -> &'static str { env!("UTIL_BUILD_NOTE") }
pub fn flavor() -> &'static str { option_env!("UTIL_FLAVOR_BAKED").unwrap_or("plain") }
pub fn compile_time() -> &'static str { option_env!("E2E_COMPILE_VAR").unwrap_or("unset") }
pub fn answer() -> i32 { mac::answer!() }
pub fn local_note() -> &'static str { include_str!("../local-note.txt").trim() }
/// ```
/// assert_eq!(util::double(2), 4);
/// ```
pub fn double(x: i32) -> i32 { x * 2 }
#[cfg(test)]
mod tests {
    #[test]
    fn answers() { assert_eq!(super::answer(), 42); }
}
EOF
cat >"$A/crates/app/Cargo.toml" <<'EOF'
[package]
name = "app"
version = "0.1.0"
edition = "2021"
[dependencies]
util = { path = "../util" }
EOF
cat >"$A/crates/app/src/main.rs" <<'EOF'
fn main() {
    println!("{} / {} / {} / {} / {}", util::GENERATED, util::note(), util::flavor(), util::answer(), util::compile_time());
    println!("args: {:?}", std::env::args().skip(1).collect::<Vec<_>>());
    println!("fixture via manifest dir: {}", std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixture.txt").exists());
    println!("note: {}", util::local_note());
}
EOF
cat >"$A/crates/app/tests/it.rs" <<'EOF'
#[test]
fn runs_the_binary() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_app")).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("from build script"));
}
#[test]
fn reads_a_fixture() {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixture.txt");
    assert_eq!(std::fs::read_to_string(p).unwrap().trim(), "fixture");
}
#[test]
fn reads_a_large_file() {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/large.bin");
    assert_eq!(std::fs::read(p).unwrap().len(), 3000000);
}
#[test]
fn writes_a_snapshot() {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/written-by-test.txt");
    std::fs::write(p, "snapshot\n").unwrap();
}
EOF
echo fixture >"$A/crates/app/tests/fixture.txt"
# A file git ignores but the build reads, and a large one a test reads.
printf '/target\nlocal-note.txt\n/junk\n' >"$A/.gitignore"
echo "ignored by git" >"$A/crates/util/local-note.txt"
head -c 3000000 /dev/urandom >"$A/crates/app/tests/large.bin"
mkdir -p "$A/junk" && head -c 2000000 /dev/urandom >"$A/junk/blob"
git -C "$A" init -q -b main
git -C "$A" add -A
git -C "$A" -c user.name=t -c user.email=t@t commit -q -m init

echo "== first build"
cd "$A"
out=$("$R" build 2>&1); rc=$?
[ $rc -eq 0 ] && ok "build succeeds" || bad "build succeeds" "$out"
has "$out" "Compiling app v0.1.0 ($A/crates/app)" && ok "status lines show real paths" || bad "status lines show real paths" "$out"
has "$out" "warning: util@0.1.0: hello from the util build script" && ok "build script warning shown" || bad "build script warning shown" "$out"
has "$out" "Finished \`dev\` profile [unoptimized + debuginfo] target(s) in" && ok "finished line" || bad "finished line" "$out"
[ -x target/debug/app ] && ok "binary linked into target/" || bad "binary linked into target/"
out=$(./target/debug/app 2>&1)
has "$out" "from build script / built-by-script / plain / 42 / unset" && ok "binary output" || bad "binary output" "$out"
has "$out" "note: ignored by git" && ok "a file git ignores is synced and built with" || bad "a file git ignores is synced and built with" "$out"
[ -w target/debug/app ] || [ "$(stat -c %h target/debug/app)" -gt 1 ] && ok "the artifact is a reflink or a link into the client's cache" || bad "the artifact is a reflink or a link into the client's cache" "$(ls -la target/debug/app)"
[ -f Cargo.lock ] && ok "Cargo.lock written to the worktree" || bad "Cargo.lock written to the worktree"
n=$(find target -type f ! -name "*.d" ! -name CACHEDIR.TAG | wc -l)
[ -f target/CACHEDIR.TAG ] && ok "target/ is tagged as a cache directory, as cargo does" || bad "target/ is tagged as a cache directory"
[ "$n" -le 4 ] && ok "target/ holds only requested artifacts ($n files)" || bad "target/ holds only requested artifacts" "$(find target -type f)"
git -C "$A" add Cargo.lock && git -C "$A" -c user.name=t -c user.email=t@t commit -q -m lock

echo "== no-op rebuild"
# The lockfile the first build created is new to the planner: one re-plan.
"$R" build >/dev/null 2>&1
before=$(runs)
out=$("$R" build 2>&1)
after=$(runs)
[ "$before" = "$after" ] && ok "nothing ran ($after actions total)" || bad "nothing ran" "before=$before after=$after"
has "$out" "Compiling" && bad "no Compiling lines" "$out" || ok "no Compiling lines"

echo "== second worktree at the same commit"
B=$T/b
git -C "$A" worktree add -q "$B" -b other
cd "$B"
# git does not check out ignored files; the build needs this one.
cp "$A/crates/util/local-note.txt" crates/util/
before=$(runs); files=$(asked_files); trees=$(asked_trees)
start=$(date +%s%N)
out=$("$R" build 2>&1); rc=$?
ms=$(( ($(date +%s%N) - start) / 1000000 ))
after=$(runs)
# Its branch is another one: what stands in for .git differs in HEAD.
[ $(( $(asked_files) - files )) -eq 1 ] && ok "the second worktree uploaded one file: the HEAD naming its branch" || bad "the second worktree uploaded one file: the HEAD naming its branch" "before=$files after=$(asked_files)"
[ $(( $(asked_trees) - trees )) -le 4 ] && ok "and $(( $(asked_trees) - trees )) directory manifests (the root and those on the way to its branch)" || bad "the second worktree uploaded only the manifests on the way to its branch" "before=$trees after=$(asked_trees)"
[ $rc -eq 0 ] && [ "$before" = "$after" ] && ok "second worktree built with zero actions in ${ms}ms" || bad "second worktree built with zero actions" "rc=$rc before=$before after=$after
$out"
[ "$(stat -c %i "$A/target/debug/app")" = "$(stat -c %i "$B/target/debug/app")" ] && ok "artifact shares an inode with the first worktree (no extra disk)" || {
    # On reflink filesystems the inodes differ but the extents are shared.
    cmp -s "$A/target/debug/app" "$B/target/debug/app" && ok "artifact identical to the first worktree's" || bad "artifact identical to the first worktree's"
}
out=$(./target/debug/app 2>&1)
has "$out" "from build script" && ok "second worktree's binary runs" || bad "second worktree's binary runs" "$out"

echo "== one-file edit rebuilds only dependents"
cd "$B"
sed -i 's/pub fn double(x: i32) -> i32 { x \* 2 }/pub fn double(x: i32) -> i32 { x + x }/' crates/util/src/lib.rs
before=$(runs); files=$(asked_files); trees=$(asked_trees)
out=$("$R" build 2>&1); rc=$?
after=$(runs)
delta=$((after - before))
[ $(( $(asked_files) - files )) -eq 1 ] && [ $(( $(asked_trees) - trees )) -eq 4 ] && ok "the edit uploaded one file and the four manifests on its path" || bad "the edit uploaded one file and the four manifests on its path" "files $files -> $(asked_files), manifests $trees -> $(asked_trees)"
[ $rc -eq 0 ] && [ "$delta" -eq 2 ] && ok "edit to util reran util and app only ($delta actions)" || bad "edit to util reran util and app only" "delta=$delta
$out"
has "$out" "Compiling mac" && bad "proc macro not rebuilt" "$out" || ok "proc macro not rebuilt"
cd "$A"
before=$(runs); "$R" build >/dev/null 2>&1; after=$(runs)
[ "$before" = "$after" ] && ok "first worktree unaffected by the other's edit" || bad "first worktree unaffected by the other's edit"

echo "== comment-only edit in a dependency does not relink dependents needlessly"
cd "$B"
echo "// trailing comment" >>crates/mac/src/lib.rs
before=$(runs)
"$R" build >/dev/null 2>&1
after=$(runs)
delta=$((after - before))
[ "$delta" -ge 1 ] && [ "$delta" -le 3 ] && ok "comment edit ran $delta action(s), without re-planning" || bad "comment edit ran a bounded number of actions" "delta=$delta"

echo "== environment variables"
cd "$A"
before=$(runs)
out=$(UTIL_FLAVOR=spicy "$R" build 2>&1); rc=$?
after=$(runs)
[ $rc -eq 0 ] && [ $((after - before)) -ge 3 ] && ok "declared variable caused a rebuild ($((after - before)) actions)" || bad "declared variable caused a rebuild" "$out"
out=$(./target/debug/app 2>&1)
has "$out" "/ spicy /" && ok "build script saw UTIL_FLAVOR" || bad "build script saw UTIL_FLAVOR" "$out"
before=$(runs); UTIL_FLAVOR=spicy "$R" build >/dev/null 2>&1; after=$(runs)
[ "$before" = "$after" ] && ok "same value again is a hit" || bad "same value again is a hit"
before=$(runs); UNRELATED_VAR=x "$R" build >/dev/null 2>&1; after=$(runs)
[ "$before" = "$after" ] && ok "unrelated variable changes nothing" || bad "unrelated variable changes nothing"
out=$(./target/debug/app 2>&1)
has "$out" "/ plain /" && ok "without the variable the plain build is back" || bad "without the variable the plain build is back" "$out"
out=$(E2E_COMPILE_VAR=seen "$R" build 2>&1 && ./target/debug/app)
has "$out" "/ seen" && ok "option_env! variable reaches rustc once declared" || bad "option_env! variable reaches rustc once declared" "$out"

echo "== tests"
cd "$A"
out=$("$R" test 2>&1); rc=$?
[ $rc -eq 0 ] && ok "tests pass" || bad "tests pass" "$out"
has "$out" "Running tests/it.rs (target/debug/" && ok "test status lines" || bad "test status lines" "$out"
has "$out" "Doc-tests util" && ok "doctests run" || bad "doctests run" "$out"
[ "$(cat crates/app/tests/written-by-test.txt 2>/dev/null)" = "snapshot" ] && ok "file written by a test appears in the worktree" || bad "file written by a test appears in the worktree"
# The file the test wrote is new to the worktree, so what read that
# directory (the planner, the test itself) runs once more and then settles.
before=$(runs); "$R" test >/dev/null 2>&1; after=$(runs)
[ $((after - before)) -le 2 ] && ok "after a test wrote into the worktree, $((after - before)) action(s) reran once" || bad "bounded reruns after a test wrote into the worktree" "delta=$((after - before))"
before=$(runs)
out=$("$R" test 2>&1)
after=$(runs)
[ "$before" = "$after" ] && has "$out" "test result: ok. 4 passed" && ok "second test run replayed from cache" || bad "second test run replayed from cache" "before=$before after=$after
$out"
echo changed >crates/app/tests/fixture.txt
before=$(runs)
out=$("$R" test 2>&1); rc=$?
after=$(runs)
[ $rc -ne 0 ] && [ $((after - before)) -eq 1 ] && has "$out" "error: test failed, to rerun pass \`-p app --test it\`" && ok "changed fixture reran exactly the test that reads it, and it failed" || bad "changed fixture reran exactly the test that reads it" "rc=$rc delta=$((after - before))
$out"
echo fixture >crates/app/tests/fixture.txt
out=$("$R" test 2>&1); rc=$?
[ $rc -eq 0 ] && ok "restored fixture passes again" || bad "restored fixture passes again" "$out"
before=$(runs); "$R" test --no-cache >/dev/null 2>&1; after=$(runs)
[ $((after - before)) -ge 3 ] && ok "--no-cache really runs the tests ($((after - before)) actions)" || bad "--no-cache really runs the tests" "delta=$((after - before))"

echo "== errors"
cd "$A"
cp crates/app/src/main.rs "$T/main.rs.bak"
echo 'fn broken() -> i32 { "not a number" }' >>crates/app/src/main.rs
out=$("$R" build 2>&1); rc=$?
[ $rc -eq 101 ] && ok "compile error exits 101" || bad "compile error exits 101" "rc=$rc"
has "$out" "error[E0308]" && has "$out" "crates/app/src/main.rs" && ok "diagnostic shown with relative path" || bad "diagnostic shown with relative path" "$out"
has "$out" 'error: could not compile `app` (bin "app") due to 1 previous error' && ok "cargo's summary line" || bad "cargo's summary line" "$out"
cp "$T/main.rs.bak" crates/app/src/main.rs
expect "fixed source builds again" "$R" build

echo "== run"
cd "$A/crates/app"
out=$("$R" run -q -- one two 2>&1); rc=$?
[ $rc -eq 0 ] && has "$out" 'args: ["one", "two"]' && ok "run passes arguments" || bad "run passes arguments" "$out"
has "$out" "fixture via manifest dir: true" && ok "compiled-in CARGO_MANIFEST_DIR resolves at run time" || bad "compiled-in CARGO_MANIFEST_DIR resolves at run time" "$out"

echo "== JSON messages"
cd "$A"
out=$("$R" build --message-format=json 2>/dev/null)
exe=$(python3 -c '
import json,sys
exe=None
n=0
for line in sys.stdin:
    m=json.loads(line); n+=1
    if m["reason"]=="compiler-artifact" and m.get("executable"): exe=m["executable"]
    last=m
assert last["reason"]=="build-finished" and last["success"] is True, last
print(exe)
' <<<"$out" 2>&1)
[ -x "$exe" ] && ok "compiler-artifact names an existing executable ($exe)" || bad "compiler-artifact names an existing executable" "$exe"

echo "== check and clippy"
cd "$A"
expect "check" "$R" check
before=$(runs); "$R" check >/dev/null 2>&1; after=$(runs)
[ "$before" = "$after" ] && ok "check is cached" || bad "check is cached"
if command -v cargo-clippy >/dev/null 2>&1; then
    out=$("$R" clippy 2>&1); rc=$?
    [ $rc -eq 0 ] && ok "clippy" || bad "clippy" "$out"
fi

echo "== concurrent cold builds from many worktrees"
C=$T/c
mkdir -p "$C"
cp -r "$A/crates" "$A/Cargo.toml" "$A/Cargo.lock" "$C/"
printf '/target\n' >"$C/.gitignore"
# A different crate name makes every action new to the cache.
sed -i 's/name = "app"/name = "app2"/' "$C/crates/app/Cargo.toml"
sed -i 's/CARGO_BIN_EXE_app/CARGO_BIN_EXE_app2/' "$C/crates/app/tests/it.rs"
sed -i 's/from build script/from another build script/' "$C/crates/util/build.rs"
sed -i 's/from build script/from another build script/' "$C/crates/app/tests/it.rs"
git -C "$C" init -q -b main && git -C "$C" add -A && git -C "$C" -c user.name=t -c user.email=t@t commit -q -m init
for i in 1 2 3 4 5 6 7; do git -C "$C" worktree add -q "$T/c$i" -b "wt$i"; done
before=$(runs)
pids=""
for d in "$C" "$T"/c[1-7]; do (cd "$d" && "$R" build >"$d/.build.log" 2>&1) & pids="$pids $!"; done
allok=1
for p in $pids; do wait "$p" || allok=0; done
after=$(runs)
delta=$((after - before))
[ $allok -eq 1 ] && ok "8 concurrent builds succeeded" || bad "8 concurrent builds succeeded" "$(cat "$C/.build.log")"
# One build needs 7 actions (plan, mac, util build script x2, util, app, and the plan is shared).
# One build is 5 new actions: plan, build script compile and run, util, app2.
# The proc macro is byte-identical to the first workspace's and is a hit.
[ "$delta" -ge 5 ] && [ "$delta" -le 6 ] && ok "8 identical cold builds ran $delta actions in total (one build's worth)" || bad "8 identical cold builds ran about one build's worth of actions" "delta=$delta"

echo "== stock cargo still works (no drift)"
cd "$A"
git -C "$A" status --porcelain | grep -v '^??' >/dev/null && bad "rargo left tracked files modified" "$(git -C "$A" status --porcelain)" || ok "no tracked file modified by rargo"
out=$(CARGO_TARGET_DIR="$T/stock-target" cargo build --offline 2>&1); rc=$?
[ $rc -eq 0 ] && ok "stock cargo builds the same worktree" || bad "stock cargo builds the same worktree" "$out"
out=$(cargo build --offline 2>&1); rc=$?
[ $rc -eq 0 ] && ok "stock cargo builds into the same target/ that rargo linked into" || bad "stock cargo builds into the same target/" "$out"
expect "and rargo still works afterwards" "$R" build

echo "== cargo's unused_dependencies lint"
cd "$A"
cp crates/app/Cargo.toml "$T/app-Cargo.toml"
mkdir -p crates/unused/src && echo "" >crates/unused/src/lib.rs
printf '[package]\nname = "unused"\nversion = "0.1.0"\nedition = "2021"\n' >crates/unused/Cargo.toml
printf 'unused = { path = "../unused" }\n' >>crates/app/Cargo.toml
out=$("$R" build 2>&1); rc=$?
stock=$(CARGO_TARGET_DIR="$T/stock-target" cargo build --offline 2>&1 | grep -v "Compiling\|Finished\|hello from")
mine=$(grep -v "Compiling\|Finished\|hello from" <<<"$out")
[ $rc -eq 0 ] && has "$out" "warning: unused dependency \`unused\`" && [ "$mine" = "$stock" ] && ok "an unused path dependency gets stock cargo's warning, word for word" || bad "an unused path dependency gets stock cargo's warning" "rargo:
$mine
cargo:
$stock"
out=$("$R" build 2>&1)
has "$out" "warning: unused dependency \`unused\`" && ok "and again when the build is replayed" || bad "and again when the build is replayed" "$out"
cp "$T/app-Cargo.toml" crates/app/Cargo.toml; rm -rf crates/unused
out=$("$R" build 2>&1)
has "$out" "unused dependency" && bad "no lint once the dependency is gone" "$out" || ok "no lint once the dependency is gone"

echo "== stock cargo can rebuild and test in the target/ rargo filled"
cd "$A"
"$R" test >/dev/null 2>&1
out=$("$R" test --no-run 2>&1); rc=$?
exe=$(grep -o "target/debug/[^)]*/it-[0-9a-f]*" <<<"$out" | head -1)
[ $rc -eq 0 ] && [ -x "$exe" ] && [ -w "$exe" ] && [ "$(stat -c %h "$exe")" = 1 ] && ok "test --no-run leaves its executables as ordinary files ($exe)" || bad "test --no-run leaves its executables as ordinary files" "$out
$(ls -la "$exe" 2>&1)"
"$R" doc >/dev/null 2>&1
out=$(cargo test --offline 2>&1); rc=$?
[ $rc -eq 0 ] && ok "stock cargo test in the same target/" || bad "stock cargo test in the same target/" "$out"
out=$(cargo doc --offline 2>&1); rc=$?
[ $rc -eq 0 ] && ok "stock cargo doc in the same target/" || bad "stock cargo doc in the same target/" "$out"
expect "and rargo test still works afterwards" "$R" test

echo "== cargo's .d files"
cd "$A"
"$R" build >/dev/null 2>&1
mine=$(cat target/debug/app.d 2>/dev/null)
CARGO_TARGET_DIR="$T/stock-target" cargo build --offline >/dev/null 2>&1
theirs=$(sed "s#$T/stock-target#$A/target#g" "$T/stock-target/debug/app.d" 2>/dev/null)
[ -n "$mine" ] && [ "$mine" = "$theirs" ] && ok "target/debug/app.d is what stock cargo writes" || bad "target/debug/app.d is what stock cargo writes" "rargo: $mine
cargo: $theirs"
mine=$(CARGO_BUILD_DEP_INFO_BASEDIR="$A" "$R" build >/dev/null 2>&1; cat target/debug/app.d)
has "$mine" "target/debug/app: crates/app/src/main.rs" && ok "build.dep-info-basedir makes the paths relative" || bad "build.dep-info-basedir makes the paths relative" "$mine"
"$R" build >/dev/null 2>&1

echo "== garbage collection keeps everything still needed"
cd "$A"
out=$("$R" --rargo-gc 2>&1)
has "$out" "collected" && ok "gc ran: $out" || bad "gc ran" "$out"
before=$(runs); "$R" build >/dev/null 2>&1; "$R" test >/dev/null 2>&1; after=$(runs)
[ "$before" = "$after" ] && ok "after gc, build and test are still fully cached" || bad "after gc, build and test are still fully cached" "delta=$((after - before))"
cd "$B"
before=$(runs); "$R" build >/dev/null 2>&1; after=$(runs)
[ "$before" = "$after" ] && ok "after gc, the other worktree is still fully cached" || bad "after gc, the other worktree is still fully cached" "delta=$((after - before))"
echo "// post-gc edit" >>crates/util/src/lib.rs
out=$("$R" build 2>&1); rc=$?
[ $rc -eq 0 ] && ok "after gc, an incremental rebuild works" || bad "after gc, an incremental rebuild works" "$out"

echo "== cold tier: idle content is kept as chunks and comes back on demand"
cd "$A"
"$R" build >/dev/null 2>&1; "$R" test >/dev/null 2>&1
out=$("$R" --rargo-compact 2>&1)
n=$(sed -n 's/^\([0-9][0-9]*\) blobs went cold.*/\1/p' <<<"$out")
[ "${n:-0}" -gt 0 ] && ok "compaction: $out" || bad "compaction moved blobs to the cold tier" "$out"
before=$(runs); "$R" build >/dev/null 2>&1; "$R" test >/dev/null 2>&1; after=$(runs)
[ "$before" = "$after" ] && ok "after compaction, build and test are still fully cached" || bad "after compaction, build and test are still fully cached" "delta=$((after - before))"
C2="$T/wt-cold"
git -C "$A" worktree add -q --detach "$C2" HEAD 2>/dev/null
cd "$C2"
cp "$A/crates/util/local-note.txt" crates/util/
before=$(runs); out=$("$R" build 2>&1); rc=$?; after=$(runs)
[ $rc -eq 0 ] && ok "a fresh worktree builds from the cold tier" || bad "a fresh worktree builds from the cold tier" "$out"
out=$(./target/debug/app 2>&1); rc=$?
[ $rc -eq 0 ] && ok "the binary put together from chunks runs" || bad "the binary put together from chunks runs" "$out"
echo "// post-compaction edit" >>crates/util/src/lib.rs
out=$("$R" build 2>&1); rc=$?
[ $rc -eq 0 ] && ok "after compaction, an incremental rebuild works" || bad "after compaction, an incremental rebuild works" "$out"
"$R" --rargo-compact >/dev/null 2>&1
out=$("$R" --rargo-gc 2>&1)
has "$out" "collected" && ok "gc with a cold tier: $out" || bad "gc with a cold tier" "$out"
cd "$A"
before=$(runs); "$R" build >/dev/null 2>&1; "$R" test >/dev/null 2>&1; after=$(runs)
[ "$before" = "$after" ] && ok "after compaction and gc, build and test are still fully cached" || bad "after compaction and gc, build and test are still fully cached" "delta=$((after - before))"

echo "== doc"
cd "$A"
out=$("$R" doc 2>&1); rc=$?
[ $rc -eq 0 ] && has "$out" "Generated $A/target/doc/" && ok "doc ends with cargo's Generated line and a real path" || bad "doc ends with cargo's Generated line and a real path" "$out"
[ -f "$A/target/doc/app/index.html" ] && [ -f "$A/target/doc/util/index.html" ] && ok "documentation is in target/doc" || bad "documentation is in target/doc" "$(ls "$A/target/doc" 2>&1)"
crates=$(head -1 "$A/target/doc/crates.js" 2>/dev/null)
has "$crates" '"app"' && has "$crates" '"mac"' && has "$crates" '"util"' && ok "the cross-crate files cover every crate ($crates)" || bad "the cross-crate files cover every crate" "$crates"
cat >"$T/fake-browser" <<EOF
#!/bin/sh
echo "\$@" >"$T/opened"
EOF
chmod +x "$T/fake-browser"
rm -f "$T/opened"
out=$(BROWSER="$T/fake-browser" "$R" doc --open -p app 2>&1); rc=$?
opened=$(cat "$T/opened" 2>/dev/null)
[ $rc -eq 0 ] && has "$out" "Opening $A/target/doc/app/index.html" && [ "$opened" = "$A/target/doc/app/index.html" ] && [ -f "$opened" ] && ok "doc --open starts the browser on the real file" || bad "doc --open starts the browser on the real file" "$out
opened: $opened"

echo "== the cargo shim"
cd "$A"
mkdir -p "$T/shim"
ln -sf "$R" "$T/shim/cargo"
reqs() { "$R" --rargo-status | awk '/^requests/ {print $2}'; }
before=$(reqs); out=$(PATH="$T/shim:$PATH" cargo build 2>&1); rc=$?; after=$(reqs)
[ $rc -eq 0 ] && [ "$after" -gt "$before" ] && ok "'cargo build' through the shim is served by rargod" || bad "'cargo build' through the shim is served by rargod" "$out"
out=$(PATH="$T/shim:$PATH" cargo metadata --format-version 1 --no-deps 2>&1); rc=$?
[ $rc -eq 0 ] && has "$out" '"workspace_root"' && ok "other subcommands go to the real cargo" || bad "other subcommands go to the real cargo" "$out"
out=$(PATH="$T/shim:$PATH" cargo --version 2>&1)
has "$out" "cargo 1." && ok "'cargo --version' is the real cargo's" || bad "'cargo --version' is the real cargo's" "$out"
out=$(PATH="$T/shim:$PATH" RARGO_SERVER=127.0.0.1:1 RARGO_FALLBACK=cargo CARGO_TARGET_DIR="$T/shim-target" cargo build --offline 2>&1); rc=$?
[ $rc -eq 0 ] && [ -x "$T/shim-target/debug/app" ] && ok "without a server the shim falls back to the real cargo" || bad "without a server the shim falls back to the real cargo" "$out"
out=$(RARGO_SERVER=127.0.0.1:1 "$R" build 2>&1); rc=$?
[ $rc -eq 101 ] && has "$out" "cannot reach the rargo server" && ok "without a server and without the fallback, the error says so" || bad "without a server and without the fallback, the error says so" "$out"

echo "== policy file (.rargo.toml)"
cd "$A"
port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
python3 -c "
import socket, sys
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(('127.0.0.1', $port)); s.listen(8)
while True:
    c, _ = s.accept(); c.sendall(b'hello'); c.close()
" &
listener=$!
cat >crates/app/tests/policy.rs <<EOF
#[test]
fn probe() {
    println!("env={:?}", std::env::var("POLICY_PROBE").ok());
    let net = std::net::TcpStream::connect(("127.0.0.1", $port)).is_ok();
    println!("net={net}");
}
EOF
out=$(POLICY_PROBE=seen "$R" test -p app --test policy -- --nocapture 2>&1)
has "$out" "env=None" && has "$out" "net=false" && ok "by default a test sees neither an undeclared variable nor the host's network" || bad "by default a test sees neither an undeclared variable nor the host's network" "$out"
cat >.rargo.toml <<'EOF'
[env]
pass = ["POLICY_PROBE"]
[network]
test-packages = ["app"]
EOF
out=$(POLICY_PROBE=seen "$R" test -p app --test policy -- --nocapture 2>&1)
has "$out" 'env=Some("seen")' && has "$out" "net=true" && ok "the policy file passes the variable and gives the package's tests the network" || bad "the policy file passes the variable and gives the package's tests the network" "$out"
out=$(POLICY_PROBE=other "$R" test -p app --test policy -- --nocapture 2>&1)
has "$out" 'env=Some("other")' && ok "a passed variable is part of the key (new value, new run)" || bad "a passed variable is part of the key" "$out"
echo 'bogus = 1' >>.rargo.toml
out=$("$R" build 2>&1); rc=$?
[ $rc -ne 0 ] && has "$out" ".rargo.toml" && ok "a malformed policy file is an error that names the file" || bad "a malformed policy file is an error that names the file" "$out"
rm -f .rargo.toml crates/app/tests/policy.rs
kill $listener 2>/dev/null
# What the policy excludes is never uploaded.
mkdir -p big-data && head -c 1500000 /dev/urandom >big-data/blob
printf '[sync]\nexclude = ["big-data"]\n' >.rargo.toml
files=$(asked_files)
expect "a build with an excluded directory" "$R" build
[ $(( $(asked_files) - files )) -eq 1 ] && ok "only the policy file itself was uploaded, not the excluded directory" || bad "only the policy file itself was uploaded, not the excluded directory" "files $files -> $(asked_files)"
rm -rf .rargo.toml big-data
expect "without the policy file the workspace builds again" "$R" build

echo "== build scripts that ask git about HEAD"
G=$T/g
mkdir -p "$G/src"
printf '[package]\nname = "gitinfo"\nversion = "0.1.0"\nedition = "2021"\nbuild = "build.rs"\n' >"$G/Cargo.toml"
cat >"$G/build.rs" <<'EOF'
use std::process::Command;
fn git(args: &[&str]) -> String {
    match Command::new("git").args(args).output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => "none".into(),
    }
}
fn main() {
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rustc-env=G_HEAD={}", git(&["rev-parse", "HEAD"]));
    println!("cargo:rustc-env=G_BRANCH={}", git(&["rev-parse", "--abbrev-ref", "HEAD"]));
    println!("cargo:rustc-env=G_SUBJECT={}", git(&["log", "-1", "--format=%s"]));
    println!("cargo:rustc-env=G_DESCRIBE={}", git(&["describe", "--always", "--dirty"]));
    println!("cargo:rustc-env=G_COUNT={}", git(&["rev-list", "--count", "HEAD"]));
}
EOF
cat >"$G/src/main.rs" <<'EOF'
fn main() {
    println!("{}|{}|{}|{}|{}", env!("G_HEAD"), env!("G_BRANCH"), env!("G_SUBJECT"), env!("G_DESCRIBE"), env!("G_COUNT"));
}
EOF
printf '/target\n' >"$G/.gitignore"
git -C "$G" init -q -b trunk && git -C "$G" add -A && git -C "$G" -c user.name=t -c user.email=t@t commit -q -m "first commit"
cd "$G"
"$R" build >/dev/null 2>&1   # writes Cargo.lock
git -C "$G" add -A && git -C "$G" -c user.name=t -c user.email=t@t commit -q -m "second commit"
head1=$(git -C "$G" rev-parse HEAD)
out=$("$R" run -q 2>&1); rc=$?
[ $rc -eq 0 ] && [ "$out" = "$head1|trunk|second commit|${head1:0:7}|1" ] && ok "the build script saw HEAD, the branch and a clean tree, and no history ($out)" || bad "the build script saw HEAD, the branch and a clean tree, and no history" "$out"
[ -d "$G/.git/objects" ] && ok "the client's own .git is untouched" || bad "the client's own .git is untouched"
echo "// dirty" >>src/main.rs
out=$("$R" run -q 2>&1)
has "$out" "|${head1:0:7}-dirty|" && ok "an uncommitted edit shows as dirty" || bad "an uncommitted edit shows as dirty" "$out"
git -C "$G" -c user.name=t -c user.email=t@t commit -q -am "third commit"
head2=$(git -C "$G" rev-parse HEAD)
out=$("$R" run -q 2>&1)
[ "$out" = "$head2|trunk|third commit|${head2:0:7}|1" ] && ok "a new commit is picked up" || bad "a new commit is picked up" "$out"
size=$(du -sk "$RARGO_CACHE_DIR"/git/* | sort -n | tail -1 | cut -f1)
[ "$size" -lt 100 ] && ok "what stands in for .git on the server is small (${size} KiB for the largest commit built)" || bad "what stands in for .git on the server is small" "${size} KiB"
printf '[sync]\ngit = false\n' >.rargo.toml
out=$("$R" run -q 2>&1)
[ "$out" = "none|none|none|none|none" ] && ok "with sync.git = false the sandbox has no repository" || bad "with sync.git = false the sandbox has no repository" "$out"
mv .rargo.toml "$T/unused-policy"
out=$(CARGO_TARGET_DIR="$T/stock-g" cargo run -q --offline 2>&1)
has "$out" "$head2|trunk|third commit|${head2:0:7}|" && ok "stock cargo on the client agrees about HEAD ($out)" || bad "stock cargo on the client agrees about HEAD" "$out"

echo
"$R" --rargo-status
echo
if [ $fails -eq 0 ]; then echo "e2e: all $checks checks passed"; else echo "e2e: $fails of $checks checks FAILED"; fi
exit $fails
