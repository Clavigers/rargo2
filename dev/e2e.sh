#!/bin/bash
# End-to-end checks against a private build server.
#
#   dev/e2e.sh            run everything
#
# The server and the client run on this machine but share nothing except a
# TCP port: the server runs in a mount namespace in which the client's
# directory (worktrees, cache, cargo home) is an empty file system, has its
# own cargo home, and requires a token.
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
# Two users of the server: an administrator, whose token the suite uses,
# and one who is not.
mkdir -p "$RARGO_STATE"
RARGO_TOKEN=$("$bin/rargod" token create --state "$RARGO_STATE" --user e2e --admin 2>/dev/null)
export RARGO_TOKEN
guest_token=$("$bin/rargod" token create --state "$RARGO_STATE" --user guest 2>/dev/null)
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
# Files and directory manifests the server has received from clients so
# far (pushed by their daemons or asked for).
asked_files() { "$R" --rargo-status | sed -n 's/^sync .* manifests and \([0-9]*\) files received.*/\1/p'; }
asked_trees() { "$R" --rargo-status | sed -n 's/^sync .*states, \([0-9]*\) manifests.*/\1/p'; }

trap '"$repo/dev/rargod.sh" stop >/dev/null 2>&1' EXIT
"$repo/dev/rargod.sh" start --store "$E/store" >/dev/null || {
    cat "$RARGO_STATE/rargod.log"
    exit 1
}

echo "== client and server are apart"
pid=$(pgrep -f "rargod --state $RARGO_STATE " | head -1)
echo marker >"$T/marker"
[ -n "$pid" ] && [ -d "/proc/$pid/root$T" ] && [ -z "$(ls -A "/proc/$pid/root$T" 2>/dev/null)" ] && ok "the server sees an empty directory where the client's files are" || bad "the server sees an empty directory where the client's files are" "$(ls -A "/proc/$pid/root$T" 2>&1)"
out=$(RARGO_TOKEN="${RARGO_TOKEN%%.*}.wrong-secret-0123456789" "$R" --rargo-status 2>&1); rc=$?
[ $rc -ne 0 ] && has "$out" "rargo auth" && ok "a token with a wrong secret is refused" || bad "a token with a wrong secret is refused" "$out"
out=$(RARGO_TOKEN=t-nobody000000.some-secret-0123456789 "$R" --rargo-status 2>&1); rc=$?
[ $rc -ne 0 ] && ok "a token the server does not know is refused" || bad "a token the server does not know is refused" "$out"
out=$(RARGO_TOKEN=$guest_token "$R" --rargo-gc 2>&1); rc=$?
[ $rc -ne 0 ] && has "$out" "may not administer" && has "$out" "rargod token create --admin" && ok "a user who is not an administrator cannot collect garbage, and is told how an administrator is made" || bad "a user who is not an administrator cannot collect garbage, and is told how an administrator is made" "$out"
expect "the right token is accepted" "$R" --rargo-status

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
# ...and it is a linked worktree: its `.git` is a file, and is shown as one.
[ $(( $(asked_files) - files )) -eq 2 ] && ok "the second worktree uploaded two files: the HEAD naming its branch and its .git file" || bad "the second worktree uploaded two files: the HEAD naming its branch and its .git file" "before=$files after=$(asked_files)"
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
# (Counted before the edit: the daemon may push it as soon as it is made.)
before=$(runs); files=$(asked_files); trees=$(asked_trees)
sed -i 's/pub fn double(x: i32) -> i32 { x \* 2 }/pub fn double(x: i32) -> i32 { x + x }/' crates/util/src/lib.rs
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

echo "== the client's cargo configuration"
mkdir -p "$T/client-cargo-home"
printf '[env]\nE2E_COMPILE_VAR = "from the client cargo home"\n' >"$T/client-cargo-home/config.toml"
out=$(CARGO_HOME="$T/client-cargo-home" "$R" build 2>&1 && ./target/debug/app)
has "$out" "/ from the client cargo home" && ok "config.toml in the client's cargo home applies to its builds on the server" || bad "config.toml in the client's cargo home applies to its builds on the server" "$out"
out=$("$R" build 2>&1 && ./target/debug/app)
has "$out" "/ unset" && ok "and not to a client without it" || bad "and not to a client without it" "$out"

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

echo "== nextest on the server"
cd "$A"
# Runs of one kind of action so far (`actions by kind` in the status).
kind_runs() { "$R" --rargo-status | sed -n "s/^actions by kind.* $1 \([0-9]*\).*/\1/p"; }
command -v cargo-nextest >/dev/null 2>&1 && bad "the client has no nextest" "$(command -v cargo-nextest)" || ok "the client has no nextest"
"$R" test >/dev/null 2>&1
rustc_before=$(kind_runs rustc); scripts_before=$(kind_runs "build scripts")
out=$("$R" nextest run 2>&1); rc=$?
rustc_after=$(kind_runs rustc); scripts_after=$(kind_runs "build scripts")
[ $rc -eq 0 ] && has "$out" "Starting 5 tests across 4 binaries" && grep -qE "Summary \[ *[0-9.]+s\] 5 tests run: 5 passed" <<<"$out" && ok "rargo nextest run: nextest's summary, exit 0" || bad "rargo nextest run: nextest's summary, exit 0" "rc=$rc
$out"
[ -n "$rustc_after" ] && [ "$rustc_before" = "$rustc_after" ] && [ "$scripts_before" = "$scripts_after" ] && ok "after rargo test, every compile of rargo nextest was a cache hit (rustc runs: $rustc_before before, $rustc_after after)" || bad "after rargo test, every compile of rargo nextest was a cache hit" "rustc $rustc_before -> $rustc_after, build scripts $scripts_before -> $scripts_after"
echo changed >crates/app/tests/fixture.txt
# Every test runs: with nextest's fail-fast, how many of the others finish
# before it stops depends on timing.
out=$("$R" nextest run --no-fail-fast 2>&1); rc=$?
[ $rc -eq 100 ] && has "$out" "FAIL [" && has "$out" "app::it reads_a_fixture" && grep -qE "5 tests run: 4 passed, 1 failed" <<<"$out" && ok "a failing test: nextest's failure output and summary, exit 100" || bad "a failing test: nextest's failure output and summary, exit 100" "rc=$rc
$out"
echo fixture >crates/app/tests/fixture.txt
mkdir -p "$T/shim" && ln -sf "$R" "$T/shim/cargo"
out=$(PATH="$T/shim:$PATH" cargo nextest run -E 'test(reads_a_fixture)' 2>&1); rc=$?
[ $rc -eq 0 ] && grep -qE "1 test run: 1 passed, 4 skipped" <<<"$out" && ok "cargo nextest through the shim, with a filter expression" || bad "cargo nextest through the shim, with a filter expression" "rc=$rc
$out"
out=$("$R" nextest --version 2>&1); rc=$?
[ $rc -eq 0 ] && has "$out" "cargo-nextest 0." && ok "rargo nextest --version is the server's nextest ($(head -1 <<<"$out"))" || bad "rargo nextest --version is the server's nextest" "$out"
mkdir -p .config
printf '[profile.ci.junit]\npath = "junit.xml"\n' >.config/nextest.toml
out=$("$R" nextest run --profile ci 2>&1); rc=$?
[ $rc -eq 0 ] && grep -q '<testcase name="reads_a_fixture"' target/nextest/ci/junit.xml 2>/dev/null && ok "the JUnit file a nextest profile asks for comes back" || bad "the JUnit file a nextest profile asks for comes back" "rc=$rc
$out
$(ls -la target/nextest/ci 2>&1)"
rm -rf .config
# Stock nextest (the server's copy, on the client's PATH) with stock cargo.
mkdir -p "$T/nextest-bin" && ln -sf "$(ls "$RARGO_STATE"/nextest/*/cargo-nextest | head -1)" "$T/nextest-bin/cargo-nextest"
out=$(PATH="$T/nextest-bin:$PATH" cargo nextest run --offline 2>&1); rc=$?
[ $rc -eq 0 ] && grep -qE "5 tests run: 5 passed" <<<"$out" && ok "stock cargo nextest in the same worktree" || bad "stock cargo nextest in the same worktree" "rc=$rc
$out"
expect "and rargo nextest still works afterwards" "$R" nextest run
# rargo's test executables replaced stock's: stock's next build compiles
# them again rather than run rargo's.
out=$(PATH="$T/nextest-bin:$PATH" cargo nextest run --offline 2>&1); rc=$?
[ $rc -eq 0 ] && grep -qE "5 tests run: 5 passed" <<<"$out" && ok "and stock cargo nextest again after it" || bad "and stock cargo nextest again after it" "rc=$rc
$out"
"$R" nextest run >/dev/null 2>&1

echo "== nextest: the version a project names"
nx_project() { # directory, manifest lines, [.config/nextest.toml]
    mkdir -p "$1/src" && git init -q "$1"
    printf '[package]\nname = "%s"\nversion = "0.1.0"\nedition = "2021"\n%s' "$(basename "$1")" "$2" >"$1/Cargo.toml"
    echo '#[test] fn t() {}' >"$1/src/lib.rs"
    if [ -n "${3:-}" ]; then mkdir -p "$1/.config" && printf '%s' "$3" >"$1/.config/nextest.toml"; fi
}
nx_project "$T/nx-a" "" 'nextest-version = "0.9.100"
'
nx_project "$T/nx-b" '[package.metadata.rargo]
nextest = "0.9.120"
'
out=$(cd "$T/nx-a" && "$R" nextest --version 2>&1); rc=$?
[ $rc -eq 0 ] && has "$out" "Installing nextest 0.9.100 on the server" && has "$out" "cargo-nextest 0.9.100 (" && ok "nextest-version in .config/nextest.toml: the server installs and runs nextest 0.9.100" || bad "nextest-version in .config/nextest.toml: the server installs and runs nextest 0.9.100" "$out"
out=$(cd "$T/nx-b" && "$R" nextest --version 2>&1); rc=$?
[ $rc -eq 0 ] && has "$out" "cargo-nextest 0.9.120 (" && ok "nextest under [package.metadata.rargo]: the same server runs nextest 0.9.120 for this project" || bad "nextest under [package.metadata.rargo]: the same server runs nextest 0.9.120 for this project" "$out"
out=$(cd "$T/nx-a" && "$R" nextest run 2>&1); rc=$?
[ $rc -eq 0 ] && ! has "$out" "Installing" && grep -qE "1 test run: 1 passed" <<<"$out" && ok "the first project's tests run on its 0.9.100, already installed beside the others ($(ls "$RARGO_STATE/nextest" | grep -v newest | tr '\n' ' '))" || bad "the first project's tests run on its 0.9.100, already installed" "$out"
mkdir -p "$T/fake-nextest"
printf '#!/bin/sh\necho "cargo-nextest 0.9.120 (client 2026-01-01)"\n' >"$T/fake-nextest/cargo-nextest"
chmod +x "$T/fake-nextest/cargo-nextest"
nx_project "$T/nx-c" ""
out=$(cd "$T/nx-c" && PATH="$T/fake-nextest:$PATH" "$R" nextest --version 2>&1); rc=$?
[ $rc -eq 0 ] && has "$out" "cargo-nextest 0.9.120 (" && ! has "$out" "client 2026" && ok "a project that names no version runs the client's own nextest version, on the server" || bad "a project that names no version runs the client's own nextest version, on the server" "$out"
newest=$(cat "$RARGO_STATE/nextest/newest-release" 2>/dev/null)
out=$(cd "$T/nx-c" && "$R" nextest --version 2>&1); rc=$?
[ $rc -eq 0 ] && [ -n "$newest" ] && has "$out" "cargo-nextest $newest (" && ok "with no version named and none on the client, the newest release ($newest)" || bad "with no version named and none on the client, the newest release" "newest=$newest
$out"
nx_project "$T/nx-d" '[package.metadata.rargo]
nextest = "9.9.9"
'
out=$(cd "$T/nx-d" && "$R" nextest run 2>&1); rc=$?
[ $rc -ne 0 ] && has "$out" "nextest 9.9.9, which the \`nextest\` key of rargo's metadata in Cargo.toml asks for, cannot be installed on this server" && has "$out" "cargo-nextest-9.9.9/cargo-nextest-9.9.9-x86_64-unknown-linux-musl" && ok "a version the server cannot get: the error names it and what was tried" || bad "a version the server cannot get: the error names it and what was tried" "rc=$rc
$out"

echo "== nextest: every test a cached action"
cd "$A"
"$R" nextest run >/dev/null 2>&1
before=$(kind_runs tests)
out=$("$R" nextest run 2>&1); rc=$?
after=$(kind_runs tests)
[ $rc -eq 0 ] && [ "$before" = "$after" ] && grep -qE "5 tests run: 5 passed" <<<"$out" && has "$out" "Replayed 5 of 5 test runs from rargo's cache (0 run on the server" && ok "a second rargo nextest run replays every test and runs none" || bad "a second rargo nextest run replays every test and runs none" "rc=$rc tests run: $before -> $after
$out"
echo changed >crates/app/tests/fixture.txt
before=$(kind_runs tests)
out=$("$R" nextest run --no-fail-fast 2>&1); rc=$?
after=$(kind_runs tests)
[ $rc -eq 100 ] && [ $((after - before)) -eq 1 ] && has "$out" "FAIL [" && has "$out" "app::it reads_a_fixture" && has "$out" "Replayed 4 of 5 test runs from rargo's cache (1 run on the server" && ok "after an edit to one fixture, exactly the test that reads it runs (and fails)" || bad "after an edit to one fixture, exactly the test that reads it runs" "rc=$rc tests run: $before -> $after
$out"
echo fixture >crates/app/tests/fixture.txt
before=$(kind_runs tests)
out=$("$R" nextest run 2>&1); rc=$?
after=$(kind_runs tests)
[ $rc -eq 0 ] && [ "$before" = "$after" ] && ok "the fixture restored: its earlier passing run is replayed" || bad "the fixture restored: its earlier passing run is replayed" "rc=$rc tests run: $before -> $after
$out"
before=$(kind_runs tests)
out=$("$R" nextest run --no-cache 2>&1); rc=$?
after=$(kind_runs tests)
[ $rc -eq 0 ] && [ $((after - before)) -eq 5 ] && has "$out" "Replayed 0 of 5 test runs" && ok "--no-cache runs every test" || bad "--no-cache runs every test" "rc=$rc tests run: $before -> $after
$out"
# A test that fails on its first attempt and passes on the second.
nx_project "$T/nx-flaky" ""
cat >"$T/nx-flaky/src/lib.rs" <<'EOF'
#[test]
fn flaky() {
    assert_ne!(std::env::var("NEXTEST_ATTEMPT").as_deref(), Ok("1"), "fails on its first attempt");
}
EOF
norm() { sed -E 's/\[ *[0-9.]+s\]/[time]/g; s/run ID [0-9a-f-]+/run ID X/' | grep -E 'FLAKY|TRY|FAIL|PASS|Summary|Starting|fails on its first'; }
out=$(cd "$T/nx-flaky" && "$R" nextest run --retries 2 2>&1); rc=$?
newest=$(cat "$RARGO_STATE/nextest/newest-release")
mkdir -p "$T/nextest-newest" && ln -sf "$RARGO_STATE/nextest/$newest/cargo-nextest" "$T/nextest-newest/cargo-nextest"
stock=$(cd "$T/nx-flaky" && PATH="$T/nextest-newest:$PATH" cargo nextest run --offline --retries 2 2>&1); src=$?
[ $rc -eq 0 ] && [ $src -eq 0 ] && has "$out" "FLAKY 2/3" && [ "$(norm <<<"$out")" = "$(norm <<<"$stock")" ] && ok "a test that fails and then passes is reported flaky, as stock nextest reports it" || bad "a test that fails and then passes is reported flaky, as stock nextest reports it" "rc=$rc stock=$src
--- rargo
$(norm <<<"$out")
--- stock
$(norm <<<"$stock")"

echo "== nextest: rargo's against stock's, in the same worktree"
cd "$A"
# What may differ: timings, run IDs, thread IDs, and the lines of the
# build (cargo's and rargo's) and rargo's own.
nnorm() {
    sed -E -e 's/\[ *[0-9.]+s\]/[time]/g' -e 's/run ID [0-9a-f-]+/run ID X/' -e "s/(thread '[^']*') \([0-9]+\)/\1 (N)/g" \
        -e 's/finished in [0-9.]+s/finished in Ns/' |
        grep -vE '^ *(Compiling|Finished|Replayed|Installing|Blocking|Locking|Downloaded|Running|Executable) |^warning: util@'
}
jnorm() { sed -E -e 's/ (uuid|timestamp|time)="[^"]*"//g' -e "s/(thread &apos;[^&]*&apos;) \([0-9]+\)/\1 (N)/g" -e 's/finished in [0-9.]+s/finished in Ns/g' "$1"; }
against_stock() { # description, arguments for `nextest run`...
    local d=$1; shift
    stock=$(PATH="$T/nextest-newest:$PATH" cargo nextest run --offline "$@" 2>&1); src=$?
    out=$("$R" nextest run "$@" 2>&1); rc=$?
    if [ $rc -eq $src ] && [ "$(nnorm <<<"$out")" = "$(nnorm <<<"$stock")" ]; then
        ok "$d: output and exit code ($rc) as stock nextest's"
    else
        bad "$d: output and exit code as stock nextest's" "rargo $rc, stock $src
$(diff <(nnorm <<<"$stock") <(nnorm <<<"$out"))"
    fi
}
echo changed >crates/app/tests/fixture.txt
against_stock "a failing test" --test-threads 1
against_stock "a failing test with --no-fail-fast" --test-threads 1 --no-fail-fast
echo fixture >crates/app/tests/fixture.txt
against_stock "a filter expression" --test-threads 1 -E 'test(=reads_a_fixture) | test(answers)'
against_stock "a partition" --test-threads 1 --partition count:2/2
cd "$T/nx-flaky"
against_stock "a flaky test with retries" --test-threads 1 --retries 2
cd "$A"
# The JUnit file a profile asks for, with a failure in it.
mkdir -p .config
printf '[profile.ci.junit]\npath = "junit.xml"\nstore-success-output = true\n' >.config/nextest.toml
echo changed >crates/app/tests/fixture.txt
PATH="$T/nextest-newest:$PATH" cargo nextest run --offline --profile ci --no-fail-fast --test-threads 1 >/dev/null 2>&1; src=$?
cp target/nextest/ci/junit.xml "$T/junit-stock.xml" 2>/dev/null
rm -f target/nextest/ci/junit.xml
"$R" nextest run --profile ci --no-fail-fast --test-threads 1 >/dev/null 2>&1; rc=$?
[ $rc -eq $src ] && [ -s "$T/junit-stock.xml" ] && grep -q '<failure' target/nextest/ci/junit.xml && [ "$(jnorm target/nextest/ci/junit.xml)" = "$(jnorm "$T/junit-stock.xml")" ] && ok "the JUnit file of a run with a failure is stock nextest's (timestamps aside)" || bad "the JUnit file of a run with a failure is stock nextest's" "rargo $rc, stock $src
$(diff <(jnorm "$T/junit-stock.xml") <(jnorm target/nextest/ci/junit.xml) 2>&1 | head -30)"
echo fixture >crates/app/tests/fixture.txt
rm -rf .config

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

echo "== the first commands of many linked worktrees at once, in a new daemon"
# Linked worktrees of one repository share files in the client's cache
# (the \`.git\` file they are shown); a new daemon writes them as each
# worktree's first command scans it.
for i in $(seq 8 23); do git -C "$C" worktree add -q --detach "$T/c$i" HEAD; done
FC=$T/.rargo-cache-first
pids=""
for i in $(seq 8 23); do (cd "$T/c$i" && RARGO_CACHE_DIR=$FC "$R" check >"$T/c$i/.check.log" 2>&1) & pids="$pids $!"; done
failed=0
for p in $pids; do wait "$p" || failed=$((failed + 1)); done
[ $failed -eq 0 ] && ! grep -q "cannot reach" "$T"/c*/.check.log && ok "16 first commands at once in a new daemon all succeed" || bad "16 first commands at once in a new daemon all succeed" "$failed failed
$(grep -h -B2 -A2 error "$T"/c*/.check.log | head -20)"
RARGO_CACHE_DIR=$FC "$R" --rargo-daemon-stop >/dev/null 2>&1

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
stock=$(CARGO_TARGET_DIR="$T/stock-target" cargo build --offline 2>&1 | grep -v "Compiling\|Finished\|hello from\|Blocking waiting for file lock")
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
depinfo_rounds=0
for _ in $(seq 1 20); do
    if ! out=$(CARGO_BUILD_DEP_INFO_BASEDIR="$A" "$R" build 2>&1); then
        bad "build with relative dep-info" "$out"
        break
    fi
    mine=$(cat target/debug/app.d)
    if ! has "$mine" "target/debug/app: crates/app/src/main.rs"; then
        bad "build.dep-info-basedir makes the paths relative (round $((depinfo_rounds + 1)))" "$mine"
        break
    fi
    if ! out=$("$R" build 2>&1); then
        bad "build with default dep-info" "$out"
        break
    fi
    mine=$(cat target/debug/app.d)
    if [ "$mine" != "$theirs" ]; then
        bad "default dep-info restores the absolute paths (round $((depinfo_rounds + 1)))" "$mine"
        break
    fi
    depinfo_rounds=$((depinfo_rounds + 1))
done
[ "$depinfo_rounds" -ne 20 ] || ok "build.dep-info-basedir switches dep-info paths (20 rounds)"

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
# That command's daemon (for another server) mounted target/ in its turn.
echo "after another server" >crates/util/local-note.txt
out=$("$R" build 2>&1); rc=$?
ran=$(./target/debug/app 2>&1)
[ $rc -eq 0 ] && has "$ran" "note: after another server" && ok "after a command for another server, target/ shows this server's builds again" || bad "after a command for another server, target/ shows this server's builds again" "$out
$ran"
echo "ignored by git" >crates/util/local-note.txt
"$R" build >/dev/null 2>&1

echo "== policy in Cargo.toml ([workspace.metadata.rargo])"
cd "$A"
cp Cargo.toml "$T/Cargo.toml.before-policy"
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
cat >>Cargo.toml <<'EOF'

[workspace.metadata.rargo.env]
pass = ["POLICY_PROBE"]
[workspace.metadata.rargo.network]
test-packages = ["app"]
EOF
out=$(POLICY_PROBE=seen "$R" test -p app --test policy -- --nocapture 2>&1)
has "$out" 'env=Some("seen")' && has "$out" "net=true" && ok "the manifest's policy passes the variable and gives the package's tests the network" || bad "the manifest's policy passes the variable and gives the package's tests the network" "$out"
out=$(cargo metadata --format-version 1 --no-deps --offline 2>&1 >/dev/null); [ -z "$out" ] && ok "stock cargo reads that manifest without a word" || bad "stock cargo reads that manifest without a word" "$out"
out=$(POLICY_PROBE=other "$R" test -p app --test policy -- --nocapture 2>&1)
has "$out" 'env=Some("other")' && ok "a passed variable is part of the key (new value, new run)" || bad "a passed variable is part of the key" "$out"
echo 'bogus = 1' >>Cargo.toml
out=$("$R" build 2>&1); rc=$?
[ $rc -ne 0 ] && has "$out" "Cargo.toml: [workspace.metadata.rargo]" && ok "an unknown key in the manifest's policy is an error that names the table" || bad "an unknown key in the manifest's policy is an error that names the table" "$out"
cp "$T/Cargo.toml.before-policy" Cargo.toml
rm -f crates/app/tests/policy.rs
# Build scripts: the one the manifest names may use the network, another may not.
for c in netprobe netprobe2; do
    mkdir -p crates/$c/src
    printf '[package]\nname = "%s"\nversion = "0.1.0"\nedition = "2021"\n' $c >crates/$c/Cargo.toml
    echo "" >crates/$c/src/lib.rs
    cat >crates/$c/build.rs <<EOF
fn main() {
    let net = std::net::TcpStream::connect(("127.0.0.1", $port)).is_ok();
    println!("cargo:warning=build script net={net}");
}
EOF
done
out=$("$R" build -p netprobe -p netprobe2 2>&1)
has "$out" "netprobe@0.1.0: build script net=false" && has "$out" "netprobe2@0.1.0: build script net=false" && ok "by default no build script reaches the host's network" || bad "by default no build script reaches the host's network" "$out"
printf '\n[workspace.metadata.rargo.network]\nbuild-scripts = ["netprobe"]\n' >>Cargo.toml
out=$("$R" build -p netprobe -p netprobe2 2>&1)
has "$out" "netprobe@0.1.0: build script net=true" && has "$out" "netprobe2@0.1.0: build script net=false" && ok "the build script the manifest names under network.build-scripts reaches it, the other does not" || bad "the build script the manifest names under network.build-scripts reaches it, the other does not" "$out"
cp "$T/Cargo.toml.before-policy" Cargo.toml
out=$("$R" build -p netprobe 2>&1)
has "$out" "netprobe@0.1.0: build script net=false" && ok "and loses it again when the manifest no longer names it" || bad "and loses it again when the manifest no longer names it" "$out"
rm -rf crates/netprobe crates/netprobe2
kill $listener 2>/dev/null
# What the policy excludes is never uploaded.
# (Counted before the edit, and the directory made after it is excluded:
# the daemon may push a change as soon as it is made.)
files=$(asked_files)
printf '\n[workspace.metadata.rargo.sync]\nexclude = ["big-data"]\n' >>Cargo.toml
sleep 0.3
mkdir -p big-data && head -c 1500000 /dev/urandom >big-data/blob
expect "a build with a directory the manifest excludes" "$R" build
[ $(( $(asked_files) - files )) -eq 1 ] && ok "only the manifest itself was uploaded, not the excluded directory" || bad "only the manifest itself was uploaded, not the excluded directory" "files $files -> $(asked_files)"
cp "$T/Cargo.toml.before-policy" Cargo.toml
rm -rf big-data
expect "without the policy the workspace builds again" "$R" build

echo "== what git ignores, left out (sync.gitignored = false) and taken back (sync.include)"
# The fixture's util crate reads local-note.txt, which git ignores (the
# first build shows it is synced by default), and git ignores /junk.
echo "ignored by git" >crates/util/local-note.txt
# Counted before the edit, the ignored files changed after it (see above).
files=$(asked_files)
printf '\n[workspace.metadata.rargo.sync]\ngitignored = false\n' >>Cargo.toml
sleep 0.3
echo "changed, and still ignored" >crates/util/local-note.txt
head -c 1500000 /dev/urandom >junk/blob2
out=$("$R" build 2>&1); rc=$?
[ $rc -ne 0 ] && has "$out" "local-note.txt" && ok "with gitignored = false a file git ignores is not on the server: the build that reads it fails" || bad "with gitignored = false a file git ignores is not on the server: the build that reads it fails" "$out"
has "$out" 'note: `crates/util/local-note.txt` is not on the build server: git ignores it' && has "$out" "gitignored = false" && has "$out" '`include`' && ok "and the failure says why, naming the option and include" || bad "and the failure says why, naming the option and include" "$out"
[ $(( $(asked_files) - files )) -eq 1 ] && ok "only the manifest was uploaded: neither the ignored file nor the ignored directory" || bad "only the manifest was uploaded: neither the ignored file nor the ignored directory" "files $files -> $(asked_files)"
files=$(asked_files)
printf 'include = ["local-note.txt"]\n' >>Cargo.toml
out=$("$R" build 2>&1); rc=$?
ran=$(./target/debug/app 2>&1)
[ $rc -eq 0 ] && has "$ran" "note: changed, and still ignored" && ok "a file include names is sent although git ignores it" || bad "a file include names is sent although git ignores it" "$out
$ran"
[ $(( $(asked_files) - files )) -eq 2 ] && ok "and only it and the manifest were uploaded, not the ignored directory" || bad "and only it and the manifest were uploaded, not the ignored directory" "files $files -> $(asked_files)"
cp "$T/Cargo.toml.before-policy" Cargo.toml
echo "ignored by git" >crates/util/local-note.txt
rm -f junk/blob2
out=$("$R" build 2>&1) && has "$(./target/debug/app 2>&1)" "note: ignored by git" && ok "without the option, what git ignores is synced again" || bad "without the option, what git ignores is synced again" "$out"

echo "== workspace policy inside a Git worktree"
nested=$T/nested-policy
git init -q "$nested"
mkdir -p "$nested/rust/member/src" "$nested/src"
cat >"$nested/Cargo.toml" <<'EOF'
[package]
name = "outer"
version = "0.1.0"
edition = "2021"
[workspace]
exclude = ["rust"]
[workspace.metadata.rargo.env]
pass = ["OUTER_PROBE"]
[workspace.metadata.rargo.sync]
exclude = ["inner-only"]
EOF
cat >"$nested/rust/Cargo.toml" <<'EOF'
[workspace]
members = ["member"]
resolver = "2"
[workspace.metadata.rargo.env]
pass = ["INNER_PROBE"]
[workspace.metadata.rargo.sync]
exclude = ["outer-only"]
EOF
cat >"$nested/rust/member/Cargo.toml" <<'EOF'
[package]
name = "inner"
version = "0.1.0"
edition = "2021"
EOF
echo outer >"$nested/outer-only"
echo inner >"$nested/inner-only"
cat >"$nested/src/lib.rs" <<'EOF'
pub const MARKER: &str = include_str!("../outer-only");

#[test]
fn outer_policy() {
    assert_eq!(std::env::var("OUTER_PROBE").as_deref(), Ok("outer"));
    assert!(std::env::var_os("INNER_PROBE").is_none());
    assert_eq!(MARKER.trim(), "outer");
    assert!(!std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("inner-only").exists());
}
EOF
cat >"$nested/rust/member/src/lib.rs" <<'EOF'
#[test]
fn inner_policy() {
    assert_eq!(std::env::var("INNER_PROBE").as_deref(), Ok("inner"));
    assert!(std::env::var_os("OUTER_PROBE").is_none());
    assert_eq!(include_str!("../../../inner-only").trim(), "inner");
    assert!(!std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../outer-only").exists());
}
EOF
policy_test() ( cd "$1" && shift && INNER_PROBE=inner OUTER_PROBE=outer "$R" test --lib "$@" )
expect "the workspace member uses its enclosing manifest's environment and exclusions" policy_test "$nested/rust/member"
expect "the same daemon selects the outer workspace policy" policy_test "$nested"
expect "an explicit nested manifest selects the inner workspace policy" policy_test "$nested" --manifest-path rust/member/Cargo.toml

cat >"$nested/rust/member/src/main.rs" <<'EOF'
static PAYLOAD: [u8; 2 * 1024 * 1024] = [42; 2 * 1024 * 1024];

fn main() {
    let payload = std::hint::black_box(&PAYLOAD);
    println!("nested artifact: {}", payload[0]);
}
EOF
out=$(cd "$nested" && "$R" build --manifest-path rust/member/Cargo.toml --bin inner 2>&1); rc=$?
nested_bin=$nested/rust/target/debug/inner
[ "$rc" -eq 0 ] && [ "$(stat -c %s "$nested_bin")" -gt 1048576 ] && [ "$("$nested_bin")" = "nested artifact: 42" ] \
    && ok "the explicit nested manifest delivers a runnable multipart artifact" \
    || bad "the explicit nested manifest delivers a runnable multipart artifact" "$out"

out=$(cd "$nested/rust/member" && "$R" --rargo-drift-check 2>&1); rc=$?
[ $rc -eq 0 ] && has "$out" "stock cargo builds" && has "$out" "rargo builds" && ok "the drift check builds the nested package on both sides" || bad "the drift check builds the nested package on both sides" "$out"

echo "== build scripts that ask git about HEAD and history"
# No build so far asked git for history: none was sent.
[ ! -e "$RARGO_CACHE_DIR/git-history" ] && ok "builds whose scripts ask git nothing about history send none" || bad "builds whose scripts ask git nothing about history send none" "$(ls "$RARGO_CACHE_DIR/git-history")"
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
    println!("cargo:rustc-env=G_LAST={}", git(&["log", "-1", "--format=%(describe:tags)"]));
}
EOF
cat >"$G/src/main.rs" <<'EOF'
fn main() {
    println!("{}|{}|{}|{}|{}|{}", env!("G_HEAD"), env!("G_BRANCH"), env!("G_SUBJECT"), env!("G_DESCRIBE"), env!("G_COUNT"), env!("G_LAST"));
}
EOF
printf '/target\n' >"$G/.gitignore"
gc() { git -C "$G" -c user.name=t -c user.email=t@t "$@"; }
git -C "$G" init -q -b trunk && git -C "$G" add -A && gc commit -q -m "first commit"
gc tag -a v0.1.0 -m "first release"
cd "$G"
"$R" build >/dev/null 2>&1   # writes Cargo.lock
git -C "$G" add -A && gc commit -q -m "second commit"
gc checkout -q -b side HEAD~1 && echo side >side.txt && git -C "$G" add side.txt && gc commit -q -m "side commit" && gc tag side-light
gc checkout -q trunk && gc merge -q --no-ff -m "merge side" side
head1=$(git -C "$G" rev-parse HEAD)
# Stock cargo reruns this build script only when .git/HEAD changes: each
# of its runs starts from an empty target directory.
stock() { CARGO_TARGET_DIR=$(mktemp -d "$T/stock-g.XXXXXX") cargo run -q --offline 2>&1; }
out=$("$R" run -q 2>&1); rc=$?
want=$(stock)
[ $rc -eq 0 ] && [ "$out" = "$want" ] && [ "$out" = "$head1|trunk|merge side|v0.1.0-3-g${head1:0:7}|4|side-light-2-g${head1:0:7}" ] && ok "the build script saw HEAD, the branch, a clean tree and the history, as stock cargo does ($out)" || bad "the build script saw HEAD, the branch, a clean tree and the history, as stock cargo does" "rargo: $out
stock: $want"
[ -d "$RARGO_CACHE_DIR/git-history" ] && ok "a build script that describes gets the history" || bad "a build script that describes gets the history"
[ -d "$G/.git/objects" ] && ok "the client's own .git is untouched" || bad "the client's own .git is untouched"
echo "// dirty" >>src/main.rs
out=$("$R" run -q 2>&1)
has "$out" "|v0.1.0-3-g${head1:0:7}-dirty|" && ok "an uncommitted edit shows as dirty" || bad "an uncommitted edit shows as dirty" "$out"
gc commit -q -am "third commit"
head2=$(git -C "$G" rev-parse HEAD)
out=$("$R" run -q 2>&1)
[ "$out" = "$(stock)" ] && [ "$out" = "$head2|trunk|third commit|v0.1.0-4-g${head2:0:7}|5|side-light-3-g${head2:0:7}" ] && ok "a new commit is picked up ($out)" || bad "a new commit is picked up" "$out"
gc tag -a v0.2.0 -m "second release"
out=$("$R" run -q 2>&1)
[ "$out" = "$(stock)" ] && [ "$out" = "$head2|trunk|third commit|v0.2.0|5|v0.2.0" ] && ok "a tag made without a commit is picked up" || bad "a tag made without a commit is picked up" "$out"
size=$(du -sk "$RARGO_CACHE_DIR"/git/* | sort -n | tail -1 | cut -f1)
[ "$size" -lt 100 ] && ok "what stands in for .git on the server is small (${size} KiB for the largest commit built, history aside)" || bad "what stands in for .git on the server is small" "${size} KiB"
printf '\n[package.metadata.rargo.sync]\ngit = "head"\n' >>Cargo.toml
out=$("$R" run -q 2>&1)
[ "$out" = "$head2|trunk|third commit|v0.2.0-dirty|1|v0.2.0" ] && ok "with sync.git = \"head\", HEAD and its tags and no history ($out)" || bad "with sync.git = \"head\", HEAD and its tags and no history" "$out"
git -C "$G" checkout -q Cargo.toml
printf '\n[package.metadata.rargo.sync]\ngit = false\n' >>Cargo.toml
out=$("$R" run -q 2>&1)
[ "$out" = "none|none|none|none|none|none" ] && ok "with sync.git = false the sandbox has no repository" || bad "with sync.git = false the sandbox has no repository" "$out"
git -C "$G" checkout -q Cargo.toml

# --- reports cargo writes into target/, and clients that go away ------------
echo "== a directory removed and made again is watched again"
M=$T/m
mkdir -p "$M/src" "$M/data"
printf '[package]\nname = "remade"\nversion = "0.1.0"\nedition = "2021"\n' >"$M/Cargo.toml"
printf 'fn main() {\n    println!("{}", include_str!("../data/v.txt").trim());\n}\n' >"$M/src/main.rs"
echo one >"$M/data/v.txt"
cd "$M"
out=$("$R" run -q 2>&1); [ "$out" = one ] && ok "built ($out)" || bad "built" "$out"
"$R" run -q >/dev/null 2>&1   # the directories watched since the first scan are looked at once more
rm -rf "$M/data" && mkdir "$M/data" && echo two >"$M/data/v.txt"
out=$("$R" run -q 2>&1); [ "$out" = two ] && ok "the new directory's file is seen ($out)" || bad "the new directory's file is seen" "$out"
"$R" run -q >/dev/null 2>&1
echo three >"$M/data/v.txt"
out=$("$R" run -q 2>&1); [ "$out" = three ] && ok "and so is a later change in it ($out)" || bad "and so is a later change in it" "$out"

echo "== more directories than the daemon may watch"
# A daemon of its own, in a user namespace whose limit is 40 watches (the
# limit is per user namespace; the machine's own is not touched).
W=$T/w
WC=$T/w-cache
mkdir -p "$W/src"
printf '[package]\nname = "manydirs"\nversion = "0.1.0"\nedition = "2021"\n' >"$W/Cargo.toml"
printf 'fn main() {\n    println!("{} {}", include_str!("../d/1/x.txt").trim(), include_str!("../d/77/x.txt").trim());\n}\n' >"$W/src/main.rs"
for i in $(seq 1 80); do mkdir -p "$W/d/$i" && echo "a$i" >"$W/d/$i/x.txt"; done
limited() { (cd "$W" && RARGO_CACHE_DIR=$WC RARGO_OUTPUT=copy RARGO_SYNC_STATS=1 unshare -Ur sh -c 'echo 40 >/proc/sys/user/max_inotify_watches && exec "$0" "$@"' "$R" "$@"); }
out=$(limited run -q 2>&1); rc=$?
[ $rc -eq 0 ] && has "$out" "a1 a77" && ok "it builds" || bad "it builds" "$out"
has "$out" "directories of $W are not watched: the kernel allows 40 inotify watches per user" && has "$out" "sysctl fs.inotify.max_user_watches=" && ok "it says the directories are not watched, and the setting to raise" || bad "it says the directories are not watched, and the setting to raise" "$out"
out=$(limited run -q 2>&1)
! has "$out" "are not watched" && ok "it says so once" || bad "it says so once" "$out"
echo b1 >"$W/d/1/x.txt"; echo b77 >"$W/d/77/x.txt"
out=$(limited run -q 2>&1)
has "$out" "b1 b77" && ok "changes are seen, in watched and unwatched directories alike" || bad "changes are seen, in watched and unwatched directories alike" "$out"
looked=$(sed -n 's/.*looking at \([0-9]*\) of \([0-9]*\) directories.*/\1 \2/p' <<<"$out" | tail -1)
set -- $looked
[ -n "${1:-}" ] && [ "$1" -lt "$2" ] && [ "$1" -gt 1 ] && ok "a scan looks only at what has no watch or changed ($1 of $2 directories)" || bad "a scan looks only at what has no watch or changed" "$out"
st=$(RARGO_CACHE_DIR=$WC "$R" --rargo-daemon-status 2>&1)
has "$st" "1 known, 0 watched completely" && ok "the daemon's status says the worktree is not watched completely" || bad "the daemon's status says the worktree is not watched completely" "$st"
RARGO_CACHE_DIR=$WC "$R" --rargo-daemon-stop >/dev/null 2>&1
cd "$A"

echo "== what the server holds outlives the daemon"
K=$T/k
mkdir -p "$K/src"
printf '[package]\nname = "kept"\nversion = "0.1.0"\nedition = "2021"\n' >"$K/Cargo.toml"
printf 'fn main() {\n    println!("{}", include_str!("../note.txt").trim());\n}\n' >"$K/src/main.rs"
first="first $$ $RANDOM"
echo "$first" >"$K/note.txt"
cd "$K"
out=$("$R" run -q 2>&1); [ "$out" = "$first" ] && ok "built" || bad "built" "$out"
held=$("$R" --rargo-daemon-status 2>&1 | sed -n 's/^held *\([0-9]*\) .*/\1/p')
"$R" --rargo-daemon-stop >/dev/null 2>&1
ls "$RARGO_CACHE_DIR"/held/* >/dev/null 2>&1 && ok "a stopped daemon leaves what the server holds in the cache ($held trees and contents)" || bad "a stopped daemon leaves what the server holds in the cache"
echo "// one more line" >>src/main.rs
out=$("$R" run -q 2>&1); [ "$out" = "$first" ] && ok "a new daemon starts at once and builds" || bad "a new daemon starts at once and builds" "$out"
st=$("$R" --rargo-daemon-status 2>&1)
pushed=$(sed -n 's/^pushed *\([0-9]*\) changes, \([0-9]*\) bytes.*/\2/p' <<<"$st")
size=$(wc -c <src/main.rs)
[ -n "$pushed" ] && [ "$pushed" -gt "$size" ] && [ "$pushed" -lt $((size + 2000)) ] && ok "and offers only what changed: $pushed bytes, the edited file and the manifests on its path" || bad "and offers only what changed" "$st"
# The server lets go of a content the record says it has; the content
# comes back to the worktree. The server asks for it.
echo second >note.txt
out=$("$R" run -q 2>&1); [ "$out" = second ] && ok "built again ($out)" || bad "built again" "$out"
gc=$("$R" --rargo-gc 2>&1)
n=$(sed -n 's/.*collected \([0-9]*\) unreferenced blobs.*/\1/p' <<<"$gc")
[ "${n:-0}" -gt 0 ] && ok "the server's gc let go of what nothing refers to any more ($n contents)" || bad "the server's gc let go of what nothing refers to any more" "$gc"
"$R" --rargo-daemon-stop >/dev/null 2>&1
echo "$first" >note.txt
out=$("$R" run -q 2>&1); [ "$out" = "$first" ] && ok "a content the record wrongly says the server holds is asked for and built with" || bad "a content the record wrongly says the server holds is asked for and built with" "$out"
cd "$A"

echo "== future-incompatibility report, --timings, interrupted clients"
H=$T/h
mkdir -p "$H/old/src" "$H/app/src"
printf '[workspace]\nresolver = "2"\nmembers = ["old", "app"]\n' >"$H/Cargo.toml"
printf '[package]\nname = "old"\nversion = "0.1.0"\nedition = "2021"\n' >"$H/old/Cargo.toml"
printf '#[repr(i32, u8)]\n#[allow(conflicting_repr_hints)]\npub enum E { A = 1, B = 2 }\n' >"$H/old/src/lib.rs"
printf '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\n[dependencies]\nold = { path = "../old" }\n' >"$H/app/Cargo.toml"
printf 'fn main() { println!("{}", old::E::B as i32); }\n' >"$H/app/src/main.rs"
cd "$H" || exit 1
out=$("$R" build 2>&1); rc=$?
[ $rc -eq 0 ] && has "$out" "will be rejected by a future version of Rust: old v0.1.0" && has "$out" "cargo report future-incompatibilities --id 1" \
    && ok "a build names the packages a future Rust will reject and the report" || bad "a build names the packages a future Rust will reject and the report" "$out"
out=$(cargo report future-incompatibilities --id 1 --offline 2>&1); rc=$?
[ $rc -eq 0 ] && has "$out" "conflicting representation hints" && has "$out" "The package \`old v0.1.0 ($H/old)\`" \
    && ok "stock cargo shows the report rargo wrote" || bad "stock cargo shows the report rargo wrote" "$out"
CARGO_TARGET_DIR="$T/stock-h" cargo build --offline >/dev/null 2>&1
per_package() { python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["reports"][0]["per_package"])' "$1" 2>&1; }
want=$(per_package "$T/stock-h/.future-incompat-report.json")
got=$(per_package "$H/target/.future-incompat-report.json")
[ -n "$got" ] && [ "$got" = "$want" ] && ok "the report is the one stock cargo writes" || bad "the report is the one stock cargo writes" "rargo: $got
stock: $want"
out=$("$R" build --future-incompat-report 2>&1)
n=$(python3 -c 'import json,sys; print(len(json.load(open(sys.argv[1]))["reports"]))' "$H/target/.future-incompat-report.json" 2>&1)
has "$out" "this report can be shown with \`cargo report future-incompatibilities --id 1\`" && [ "$n" = 1 ] \
    && ok "a cached build reports the same warnings under the same number" || bad "a cached build reports the same warnings under the same number" "$out ($n reports)"
printf 'fn main() { println!("{} again", old::E::B as i32); }\n' >"$H/app/src/main.rs"
out=$("$R" build --timings 2>&1); rc=$?
page=$H/target/cargo-timings/cargo-timing.html
[ $rc -eq 0 ] && has "$out" "Timing report saved to $H/target/cargo-timings/cargo-timing-" && grep -qF 'app `app` (bin ' "$page" 2>/dev/null \
    && grep -q 'from the cache' "$page" && [ "$(ls "$H/target/cargo-timings" | wc -l)" = 2 ] \
    && ok "--timings writes a report of what ran and what came from the cache" || bad "--timings writes a report of what ran and what came from the cache" "$out
$(ls -la "$H/target/cargo-timings" 2>&1)"
# A client that was killed outright left temporaries; the next one that
# writes there removes them, and nobody else's.
true & dead=$!; wait $dead
: >"$H/target/debug/.rargo-tmp-$dead-7-app"
: >"$H/target/debug/.rargo-tmp-$$-7-app"
printf 'fn main() { println!("{} and again", old::E::B as i32); }\n' >"$H/app/src/main.rs"
"$R" build >/dev/null 2>&1
[ ! -e "$H/target/debug/.rargo-tmp-$dead-7-app" ] && [ -e "$H/target/debug/.rargo-tmp-$$-7-app" ] \
    && ok "temporaries of a killed client are removed by the next one, those of a live process are kept" \
    || bad "temporaries of a killed client are removed by the next one, those of a live process are kept" "$(ls -A "$H/target/debug")"
rm -f "$H/target/debug/.rargo-tmp-$$-7-app"
# A client that goes away takes its running actions with it.
mkdir -p "$H/slowcancel/src"
printf '[package]\nname = "slowcancel"\nversion = "0.1.0"\nedition = "2021"\nbuild = "build.rs"\n' >"$H/slowcancel/Cargo.toml"
printf 'fn main() { std::thread::sleep(std::time::Duration::from_secs(600)); }\n' >"$H/slowcancel/build.rs"
: >"$H/slowcancel/src/lib.rs"
printf '[workspace]\nresolver = "2"\nmembers = ["old", "app", "slowcancel"]\n' >"$H/Cargo.toml"
"$R" build -p slowcancel >"$T/cancel.log" 2>&1 &
client=$!
script=""
for _ in $(seq 1 300); do
    script=$(pgrep -f '^[^ ]*build/slowcancel/[0-9a-f]*/out/build_script_build[^ ]*$' | head -1)
    [ -n "$script" ] && break
    sleep 0.1
done
kill "$client" 2>/dev/null; wait "$client" 2>/dev/null
gone=no
for _ in $(seq 1 50); do
    if [ -z "$(pgrep -f '^[^ ]*build/slowcancel/[0-9a-f]*/out/build_script_build[^ ]*$')" ]; then gone=yes; break; fi
    sleep 0.1
done
[ -n "$script" ] && [ $gone = yes ] && ok "an action still running when its client goes away is stopped" \
    || bad "an action still running when its client goes away is stopped" "script pid: ${script:-never started}; $(cat "$T/cancel.log")"
left=$(find "$H/target" -name '.rargo-tmp-*' | head -3)
[ -z "$left" ] && ok "an interrupted client leaves no temporaries in target/" || bad "an interrupted client leaves no temporaries in target/" "$left"
out=$("$R" build -p app 2>&1); rc=$?
[ $rc -eq 0 ] && ok "the server builds on after an interrupted request" || bad "the server builds on after an interrupted request" "$out"

echo
"$R" --rargo-status
echo
if [ $fails -ne 0 ]; then
    echo "client daemon log:"
    grep -v ': pushing ' "$RARGO_CACHE_DIR/daemon.log" 2>/dev/null | tail -40
    echo
fi
if [ $fails -eq 0 ]; then echo "e2e: all $checks checks passed"; else echo "e2e: $fails of $checks checks FAILED"; fi
exit $fails
