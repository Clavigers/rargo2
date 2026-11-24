#!/bin/bash
# Flake-restricted sandboxes and cache portability between "machines".
#
# Two build servers with separate state directories and cargo homes, each
# with a client whose worktree is at a different path and hidden from both
# servers, stand in for two sites. A build on A is exported, imported on B,
# and B must then build the same commit without running any compiler, build
# script or test. The project has a registry dependency, so the servers
# also fetch it themselves.
#
# Needs Nix with flakes and network access for the first evaluation.
set -u
work=${RARGO_WORK:-/root/rargo-work}
repo=$(cd "$(dirname "$0")/.." && pwd)
. "$work/env.sh"
bin=${RARGO_BIN:-$repo/target/release}
export RARGO_BIN=$bin
R=$bin/rargo
T=${RARGO_E2E_FLAKE:-$work/e2e-flake}
fails=0; checks=0
ok() { checks=$((checks + 1)); echo "  ok   $1"; }
bad() { checks=$((checks + 1)); fails=$((fails + 1)); echo "  FAIL $1"; [ -n "${2:-}" ] && echo "$2" | sed 's/^/       | /' | head -30; }
has() { grep -qF -- "$2" <<<"$1"; }

base=${RARGO_E2E_FLAKE_PORT:-7471}
port() { case $1 in a) echo $base ;; b) echo $((base + 1)) ;; esac; }
daemon() { # name action
    RARGO_LOG_ACTIONS=1 RARGO_CACHE_DIR=$T/home-$1/.cache/rargo RARGO_STATE=$T/state-$1 RARGO_SERVER=127.0.0.1:$(port "$1") RARGO_SERVER_CARGO_HOME=$T/state-$1/cargo-home \
        RARGO_HIDE="$T/a $T/b $T/home-a $T/home-b" "$repo/dev/rargod.sh" "$2" >/dev/null 2>&1
}
on() { # machine, command...
    local m=$1; shift
    RARGO_SERVER=127.0.0.1:$(port "$m") RARGO_CACHE_DIR=$T/home-$m/.cache/rargo HOME=$T/home-$m CARGO_HOME=$T/home-$m/.cargo "$@"
}
runs() { on "$1" "$R" --rargo-status | awk '/^actions run/ {print $3}'; }
actions_since() { tail -n "+$(($1 + 1))" "$log" | sed -n 's/^rargod: action //p'; }

daemon a stop; daemon b stop
rm -rf "$T"; mkdir -p "$T/home-a" "$T/home-b" "$T/a" "$T/b/somewhere/else"
trap 'daemon a stop; daemon b stop' EXIT

P=$T/a/proj
mkdir -p "$P/src"
cat >"$P/flake.nix" <<'NIX'
{
  description = "rargo flake-mode test project";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  outputs = { self, nixpkgs }:
    let pkgs = nixpkgs.legacyPackages.x86_64-linux; in {
      devShells.x86_64-linux.default = pkgs.mkShell {
        packages = [ pkgs.rustc pkgs.cargo pkgs.gcc ];
      };
    };
}
NIX
# Reuse a lock file if one was made before, so the same nixpkgs is used.
[ -f "$work/flake-test/proj/flake.lock" ] && cp "$work/flake-test/proj/flake.lock" "$P/flake.lock"
cat >"$P/Cargo.toml" <<'TOML'
[package]
name = "flaky"
version = "0.1.0"
edition = "2021"
build = "build.rs"
[dependencies]
anyhow = "1"
TOML
cat >"$P/build.rs" <<'RS'
fn main() {
    let cc = std::process::Command::new("cc").arg("--version").output().expect("cc from the flake");
    let first = String::from_utf8_lossy(&cc.stdout).lines().next().unwrap_or("").to_string();
    println!("cargo:rustc-env=FLAKY_CC={first}");
}
RS
cat >"$P/src/main.rs" <<'RS'
fn main() -> anyhow::Result<()> {
    println!("built with: {}", env!("FLAKY_CC"));
    Ok(())
}
#[cfg(test)]
mod tests {
    #[test]
    fn host_is_not_visible() {
        assert!(!std::path::Path::new("/usr/lib").exists(), "host /usr/lib must not be in a flake sandbox");
        assert!(!std::path::Path::new("/root").exists() && !std::path::Path::new("/home").exists());
        assert!(std::path::Path::new("/bin/sh").exists());
        assert!(std::fs::read_to_string("/etc/passwd").unwrap().contains("root"));
    }
}
RS
printf '/target\n' >"$P/.gitignore"
git -C "$P" init -q -b main
git -C "$P" add -A
( cd "$P" && nix --extra-experimental-features nix-command --extra-experimental-features flakes flake lock >/dev/null 2>&1 )
git -C "$P" add -A
git -C "$P" -c user.name=t -c user.email=t@t commit -q -m init

echo "== site A: flake build"
daemon a start || { echo "cannot start rargod A"; exit 1; }
cd "$P"
out=$(on a "$R" build 2>&1); rc=$?
[ $rc -eq 0 ] && ok "build in a flake sandbox" || bad "build in a flake sandbox" "$out"
out=$(./target/debug/flaky 2>&1)
has "$out" "built with: gcc (GCC)" && ok "build script ran the flake's compiler ($out)" || bad "build script ran the flake's compiler" "$out"
file target/debug/flaky | grep -q "interpreter /nix/store/" && ok "binary is linked against the flake's libc" || bad "binary is linked against the flake's libc" "$(file target/debug/flaky)"
out=$(on a "$R" test 2>&1); rc=$?
[ $rc -eq 0 ] && has "$out" "1 passed" && ok "test confirms the host filesystem is not visible" || bad "test confirms the host filesystem is not visible" "$out"
git -C "$P" add Cargo.lock; git -C "$P" -c user.name=t -c user.email=t@t commit -q -m lock
on a "$R" build >/dev/null 2>&1; on a "$R" test >/dev/null 2>&1
a_hash=$(sha256sum target/debug/flaky | cut -d' ' -f1)

echo "== without the flake (policy switch) the host toolchain is used"
cp "$P/Cargo.toml" "$T/Cargo.toml.with-flake"
printf '\n[package.metadata.rargo.flake]\nenabled = false\n' >>"$P/Cargo.toml"
out=$(on a "$R" build 2>&1); rc=$?
[ $rc -eq 0 ] && file target/debug/flaky | grep -q "interpreter /lib64/" && ok "flake.enabled = false builds against the host" || bad "flake.enabled = false builds against the host" "$out $(file target/debug/flaky)"
cp "$T/Cargo.toml.with-flake" "$P/Cargo.toml"
on a "$R" build >/dev/null 2>&1
[ "$(sha256sum target/debug/flaky | cut -d' ' -f1)" = "$a_hash" ] && ok "removing the switch brings back the identical flake build" || bad "removing the switch brings back the identical flake build"

echo "== the default flake (install/flake): the build essentials"
# A project that copies rargo's default flake finds a C compiler, cmake,
# make, pkg-config and the common libraries, none of them the host's.
D=$T/a/essentials
mkdir -p "$D/src"
cp "$repo/install/flake/flake.nix" "$repo/install/flake/flake.lock" "$D/"
printf '[package]\nname = "essentials"\nversion = "0.1.0"\nedition = "2021"\nbuild = "build.rs"\n' >"$D/Cargo.toml"
cat >"$D/build.rs" <<'RS'
use std::process::Command;
fn say(cmd: &str, args: &[&str]) -> String {
    let out = Command::new(cmd).args(args).output().unwrap_or_else(|e| panic!("{cmd}: {e}"));
    assert!(out.status.success(), "{cmd} {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("").to_string()
}
fn main() {
    let libs = ["openssl", "zlib", "sqlite3"].map(|l| format!("{l} {}", say("pkg-config", &["--modversion", l])));
    let where_ssl = say("pkg-config", &["--variable=libdir", "openssl"]);
    println!("cargo:rustc-env=ESSENTIALS={}; {}; {}; {}; {}", say("cc", &["--version"]), say("cmake", &["--version"]), say("make", &["--version"]), libs.join(", "), where_ssl);
}
RS
echo 'fn main() { println!("{}", env!("ESSENTIALS")); }' >"$D/src/main.rs"
printf '/target\n' >"$D/.gitignore"
git -C "$D" init -q -b main; git -C "$D" add -A; git -C "$D" -c user.name=t -c user.email=t@t commit -q -m init
cd "$D"
out=$(on a "$R" build 2>&1); rc=$?
[ $rc -eq 0 ] && ok "a project with the default flake builds" || bad "a project with the default flake builds" "$out"
out=$(./target/debug/essentials 2>&1)
has "$out" "gcc (GCC)" && has "$out" "cmake version" && has "$out" "GNU Make" && ok "its build script finds cc, cmake and make" || bad "its build script finds cc, cmake and make" "$out"
has "$out" "openssl 3" && has "$out" "zlib 1" && has "$out" "sqlite3 3" && ok "pkg-config finds openssl, zlib and sqlite" || bad "pkg-config finds openssl, zlib and sqlite" "$out"
[[ "$out" == *"; /nix/store/"* ]] && ok "from the flake, not the host (${out##*; })" || bad "from the flake, not the host" "$out"
cd "$P"

echo "== export from A"
out=$(on a "$R" --rargo-export "$T/export" 2>&1)
has "$out" "exported" && ok "$out" || bad "export" "$out"

echo "== site B: another server, a client at a different path, separate store and cargo home"
Q=$T/b/somewhere/else/checkout
git clone -q "$P" "$Q"
daemon b start || { echo "cannot start rargod B"; exit 1; }
cd "$Q"
out=$(on b "$R" --rargo-import "$T/export" 2>&1)
has "$out" "imported" && ok "$out" || bad "import" "$out"
before=$(runs b)
log=$T/state-b/rargod.log
start_line=$(wc -l <"$log")
# Use A's command line so it can also replay the imported plan.
out=$(RARGO_EXPLAIN=1 on b "$R" build 2>&1); rc=$?
after=$(runs b)
actions=$(actions_since "$start_line")
[ $rc -eq 0 ] && ok "B builds" || bad "B builds" "$out"
# The imported cache does not populate B's Cargo registry. Offline planning
# can fail until Cargo downloads the locked dependency. After the fetch
# reuse the imported plan when its observations still match. Compiler and
# build script actions must all be cache hits.
plan_ok='Plan [cargo (planning)] exit=Some(0) tainted=false'
plan_offline='Plan [cargo (planning)] exit=Some(101) tainted=false'
reuse=
case "$((after - before)):$actions" in
    "0:"|"1:$plan_ok"|"1:$plan_offline")
        reuse="B ran $((after - before)) planning action(s): compilers and build scripts all came from A's cache" ;;
    "2:$plan_offline"$'\n'"$plan_ok")
        # The sparse index stores an HTTP cache validator alongside package
        # records. Cargo can fetch different index bytes for the exact same
        # locked crate. The planner observes the index; the build units do
        # not. Allow a second plan only when its explanation names this.
        index_change=$(grep '^note: planning runs because /c/registry/index/[^/]*/\.cache/an/yh/anyhow was File .* is now a file of ' <<<"$out")
        if [ -n "$index_change" ] && [ "$(grep '^note: planning runs because ' <<<"$out" | tail -n 1)" = "$index_change" ]; then
            reuse="B replanned because the sparse index for the locked anyhow crate changed: $index_change"
        fi ;;
esac
if [ -n "$reuse" ]; then
    ok "$reuse"
else
    bad "B reused A's cache" "ran $((after - before)) actions
$actions
$out"
fi
has "$out" "Compiling" && bad "B printed no Compiling lines" "$out" || ok "B printed no Compiling lines"
[ "$(sha256sum target/debug/flaky | cut -d' ' -f1)" = "$a_hash" ] && ok "B's binary is byte-identical to A's" || bad "B's binary is byte-identical to A's"
before=$(runs b)
start_line=$(wc -l <"$log")
out=$(RARGO_EXPLAIN=1 on b "$R" test 2>&1); rc=$?
after=$(runs b)
actions=$(actions_since "$start_line")
if [ "$rc" -eq 0 ] && { [ "$((after - before)):$actions" = '0:' ] || [ "$((after - before)):$actions" = "1:$plan_ok" ]; }; then
    ok "B's test run was replayed from A's cache"
else
    bad "B's test run was replayed from A's cache" "ran $((after - before)) actions
$actions
$out"
fi

echo "== control: a third store without the import has to build"
daemon b stop
rm -rf "$T/state-b"
daemon b start
before=$(runs b)
on b "$R" build >/dev/null 2>&1
after=$(runs b)
[ $((after - before)) -ge 4 ] && ok "without the import B ran $((after - before)) actions" || bad "without the import B has to run the build" "ran $((after - before))"

echo
if [ $fails -eq 0 ]; then echo "e2e-flake: all $checks checks passed"; else echo "e2e-flake: $fails of $checks checks FAILED"; fi
exit $fails
