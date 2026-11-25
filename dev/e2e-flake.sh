#!/bin/bash
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

ports=4
base=${RARGO_E2E_FLAKE_PORT:-7471}
port() { case $1 in a) echo $base ;; b) echo $((base + 1)) ;; esac; }
daemon() {
    RARGO_LOG_ACTIONS=1 RARGO_CACHE_DIR=$T/home-$1/.cache/rargo RARGO_STATE=$T/state-$1 RARGO_SERVER=127.0.0.1:$(port "$1") RARGO_SERVER_CARGO_HOME=$T/state-$1/cargo-home \
        RARGO_HIDE="$T/a $T/b $T/home-a $T/home-b" "$repo/dev/rargod.sh" "$2" >/dev/null 2>&1
}
on() {
    local m=$1; shift
    RARGO_SERVER=127.0.0.1:$(port "$m") RARGO_CACHE_DIR=$T/home-$m/.cache/rargo HOME=$T/home-$m CARGO_HOME=$T/home-$m/.cargo "$@"
}
runs() { on "$1" "$R" --rargo-status | awk '/^actions run/ {print $3}'; }
actions_since() { tail -n "+$(($1 + 1))" "$log" | sed -n 's/^rargod: action //p'; }

daemon a stop; daemon b stop
rm -rf "$T"; mkdir -p "$T/home-a" "$T/home-b" "$T/a" "$T/b/somewhere/else"
mkdir -p "$T/state-a/cargo-home/bin"
printf '#!/bin/sh\necho "the server cargo home bin'"'"'s cargo-clippy ran" >&2\nexit 1\n' >"$T/state-a/cargo-home/bin/cargo-clippy"
chmod +x "$T/state-a/cargo-home/bin/cargo-clippy"
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
        packages = [ pkgs.rustc pkgs.cargo pkgs.clippy pkgs.gcc ];
      };
    };
}
NIX
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
out=$(on a "$R" clippy 2>&1); rc=$?
[ $rc -eq 0 ] && ok "clippy runs the flake's clippy" || bad "clippy runs the flake's clippy" "$out"
git -C "$P" add Cargo.lock; git -C "$P" -c user.name=t -c user.email=t@t commit -q -m lock
on a "$R" build >/dev/null 2>&1; on a "$R" test >/dev/null 2>&1
a_hash=$(sha256sum target/debug/flaky | cut -d' ' -f1)

echo "== flake source and lock are in the cache key"
for input in flake.nix flake.lock; do
    cp "$P/$input" "$T/$input.saved"
    before=$(runs a)
    printf '\n' >>"$P/$input"
    out=$(on a "$R" build 2>&1); rc=$?
    after=$(runs a)
    [ "$rc" -eq 0 ] && [ "$((after - before))" -gt 1 ] && ok "editing $input invalidates the build cache" || bad "editing $input invalidates the build cache" "$out"
    cp "$T/$input.saved" "$P/$input"
    out=$(on a "$R" build 2>&1); rc=$?
    [ "$rc" -eq 0 ] && [ "$(sha256sum target/debug/flaky | cut -d' ' -f1)" = "$a_hash" ] && ok "restoring $input restores the identical build" || bad "restoring $input restores the identical build" "$out"
done

echo "== a project without a flake uses the bundled shell"
D=$T/a/essentials
mkdir -p "$D/src"
printf '[package]\nname = "essentials"\nversion = "0.1.0"\nedition = "2021"\nbuild = "build.rs"\n' >"$D/Cargo.toml"
cat >"$D/build.rs" <<'RS'
use std::process::Command;
fn say(cmd: &str, args: &[&str]) -> String {
    let out = Command::new(cmd).args(args).output().unwrap_or_else(|e| panic!("{cmd}: {e}"));
    assert!(out.status.success(), "{cmd} {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("").to_string()
}
fn main() {
    assert!(!std::path::Path::new("/usr/lib").exists());
    assert!(!std::path::Path::new("/root").exists());
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
[ $rc -eq 0 ] && ok "a project without a flake builds with the bundled shell" || bad "a project without a flake builds with the bundled shell" "$out"
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
out=$(RARGO_EXPLAIN=1 on b "$R" build 2>&1); rc=$?
after=$(runs b)
actions=$(actions_since "$start_line")
[ $rc -eq 0 ] && ok "B builds" || bad "B builds" "$out"
plan_ok='Plan [cargo (planning)] exit=Some(0) tainted=false'
plan_offline='Plan [cargo (planning)] exit=Some(101) tainted=false'
reuse=
case "$((after - before)):$actions" in
    "0:"|"1:$plan_ok"|"1:$plan_offline")
        reuse="B ran $((after - before)) planning action(s): compilers and build scripts all came from A's cache" ;;
    "2:$plan_offline"$'\n'"$plan_ok")
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

echo "== a worker with an empty Nix store builds the flake"
back=$PWD
mkdir -p "$T/nw-proj/src" "$T/state-nw" "$T/worker-nw" "$T/home-nw"
cp "$P/flake.nix" "$P/flake.lock" "$P/Cargo.toml" "$P/Cargo.lock" "$P/build.rs" "$T/nw-proj/"
cp "$P/src/"* "$T/nw-proj/src/"
nw_port=$((base + 2))
wk_port=$((base + 3))
nw_srv=
nw_worker=
trap 'kill ${nw_srv:-} ${nw_worker:-} 2>/dev/null; daemon a stop; daemon b stop' EXIT
RARGO_HEDGE=0 RARGO_DISPATCH=eager systemd-run --scope -q -p CPUQuota=400% -p MemoryMax=20G -p Delegate=yes \
    "$bin/rargod" --state "$T/state-nw" --listen "127.0.0.1:$nw_port" \
    --workers-listen "127.0.0.1:$wk_port" \
    --planner "${RARGO_PLANNER:-$work/cargo/target/release/cargo}" \
    --cargo-home "$T/state-nw/cargo-home" --jobs 1 --drift-interval-min 0 \
    >"$T/state-nw.log" 2>&1 &
nw_srv=$!
if tail -n +1 -f --pid="$nw_srv" "$T/state-nw.log" | awk '/serving clients/ { found=1; exit } END { exit found ? 0 : 1 }'; then
    libs=$(ldd "$bin/rargod" | grep -o '/nix/store/[^ ]*' | sort -u | tr '\n' ' ')
    RARGO_LD_LIBS="$libs" systemd-run --scope -q -p CPUQuota=800% -p MemoryMax=30G -p Delegate=yes \
        unshare -m --propagation private bash -c '
            set -euo pipefail
            real=$(mktemp -d)
            mount --bind /nix/store "$real"
            # Resolve before the store is covered. A library can be a symlink
            # to another store path; bind the file it names onto the path the
            # loader opens.
            pairs=
            for p in $RARGO_LD_LIBS; do
                target=$(readlink -f "$p")
                case $target in
                    /nix/store/*) ;;
                    *) echo "library $p is not in the nix store ($target)" >&2; exit 1 ;;
                esac
                pairs="$pairs $p|$target"
            done
            mount -t tmpfs -o mode=755 tmpfs /nix/store
            for pair in $pairs; do
                p=${pair%%|*}
                target=${pair#*|}
                rel=${p#/nix/store/}
                src=${target#/nix/store/}
                mkdir -p "/nix/store/$(dirname "$rel")"
                touch "/nix/store/$rel"
                mount --bind "$real/$src" "/nix/store/$rel"
            done
            umount "$real" || umount -l "$real"
            rmdir "$real" 2>/dev/null || true
            if find /nix/store -path "*/bin/rustc" -o -path "*/bin/cc" -o -path "*/bin/nix" | grep -q .; then
                echo "nix tools are still visible on the worker" >&2
                exit 1
            fi
            exec "$1" --state "$2" --worker "$3" --jobs 4
        ' bash "$bin/rargod" "$T/worker-nw" "127.0.0.1:$wk_port" >"$T/worker-nw.log" 2>&1 &
    nw_worker=$!
    joined=0
    if tail -n +1 -f --pid="$nw_worker" "$T/state-nw.log" | awk '/joined/ { found=1; exit } END { exit found ? 0 : 1 }'; then
        joined=1
    fi
    cd "$T/nw-proj"
    out=$(RARGO_SERVER=127.0.0.1:$nw_port RARGO_CACHE_DIR=$T/home-nw/.cache/rargo HOME=$T/home-nw CARGO_HOME=$T/home-nw/.cargo "$R" build 2>&1); rc=$?
    st=$(RARGO_SERVER=127.0.0.1:$nw_port RARGO_CACHE_DIR=$T/home-nw/.cache/rargo HOME=$T/home-nw "$R" --rargo-status 2>&1 || true)
    nix=$(grep -oE '[0-9.]+ nix store lookups' <<<"$st" | head -1 | cut -d' ' -f1)
    disp=$(grep -oE '[0-9.]+ actions dispatched' <<<"$st" | head -1 | cut -d' ' -f1)
    if [ "$rc" -eq 0 ] && [ "$joined" = 1 ] && [ "${disp:-0}" -ge 1 ] && [ "${nix:-x}" = 0 ]; then
        ok "a worker that cannot see /nix/store built the flake ($disp actions, $nix nix store lookups)"
    else
        bad "a worker that cannot see /nix/store built the flake" "$out
joined=$joined dispatched=${disp:-} nix=${nix:-}
$st
$(tail -40 "$T/state-nw.log")
$(tail -40 "$T/worker-nw.log")"
    fi
    grep "nix closure:" "$T/state-nw.log" | sed 's/^/        /' || true
else
    bad "a worker that cannot see /nix/store built the flake" "the server did not start
$(tail -40 "$T/state-nw.log")"
fi
kill "$nw_srv" ${nw_worker:-} 2>/dev/null || true
wait "$nw_srv" ${nw_worker:-} 2>/dev/null || true
nw_srv=
nw_worker=
cd "$back"
trap 'daemon a stop; daemon b stop' EXIT

echo "== a server without Nix rejects a build"
daemon b stop
PATH="$T/no-tools" "$bin/rargod" --state "$T/no-nix" --listen "127.0.0.1:$(port b)" \
    --planner "${RARGO_PLANNER:-$work/cargo/target/release/cargo}" --jobs 1 >"$T/no-nix.log" 2>&1 &
no_nix=$!
trap 'kill "$no_nix" 2>/dev/null; wait "$no_nix" 2>/dev/null; daemon a stop; daemon b stop' EXIT
tail -n +1 -f --pid="$no_nix" "$T/no-nix.log" | awk '/serving clients/ { found=1; exit } END { exit found ? 0 : 1 }' || true
out=$(on b "$R" build 2>&1); rc=$?
[ "$rc" -ne 0 ] && has "$out" "rargod needs Nix with flakes to build" \
    && ok "the client is told that the server requires Nix with flakes" \
    || bad "the client is told that the server requires Nix with flakes" "$out
$(cat "$T/no-nix.log")"

echo
if [ $fails -eq 0 ]; then echo "e2e-flake: all $checks checks passed"; else echo "e2e-flake: $fails of $checks checks FAILED"; fi
exit $fails
