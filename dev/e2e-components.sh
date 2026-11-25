#!/bin/bash
set -u
work=${RARGO_WORK:-/root/rargo-work}
repo=$(cd "$(dirname "$0")/.." && pwd)
. "$work/env.sh"
bin=${RARGO_BIN:-$repo/target/release}
export RARGO_BIN=$bin
R=$bin/rargo
FILE_TC=1.88.0

fails=0
checks=0
ok() { checks=$((checks + 1)); echo "  ok   $1"; }
bad() { checks=$((checks + 1)); fails=$((fails + 1)); echo "  FAIL $1"; shift; printf '%s\n' "$@" | sed 's/^/        /' | head -40; }
check() { local rc=$1 name=$2; shift 2; if [ "$rc" = 0 ]; then ok "$name"; else bad "$name" "$@"; fi; }

RARGO_STATE=$(mktemp -d "${RARGO_E2E_ROOT:-$work}/e2e-components-state.XXXXXXXX") || exit 1
E=$(mktemp -d "${RARGO_E2E_ROOT:-$work}/e2e-components.XXXXXXXX") || exit 1
T=$E/client
mkdir -p "$T" "$E/store"
export RARGO_STATE RARGO_HIDE=$T RARGO_CACHE_DIR=$T/.rargo-cache
export RARGO_SERVER_CARGO_HOME=$RARGO_STATE/cargo-home
port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
export RARGO_SERVER=127.0.0.1:$port
RARGO_TOKEN=$("$bin/rargod" token create --state "$RARGO_STATE" --user e2e --admin 2>/dev/null)
export RARGO_TOKEN
unset RUSTUP_TOOLCHAIN CARGO_TARGET_DIR
cleanup() {
    "$repo/dev/rargod.sh" stop >/dev/null 2>&1
    rm -rf "$E" "$RARGO_STATE"
}
trap cleanup EXIT
"$repo/dev/rargod.sh" start --store "$E/store" >/dev/null || {
    cat "$RARGO_STATE/rargod.log"
    exit 1
}

project() {
    mkdir -p "$1/src"
    (
        cd "$1" || exit 1
        git init -q .
        printf '[package]\nname = "lintme"\nversion = "0.1.0"\nedition = "2021"\n' >Cargo.toml
        printf 'fn main() {\n    let v = vec![1];\n    if v.len() == 0 {\n        println!("empty");\n    }\n}\n' >src/main.rs
        [ -n "${2:-}" ] && printf '%s' "$2" >rust-toolchain.toml
        git add -A
        git -c user.name=t -c user.email=t@t.invalid commit -qm one
    )
}
linted() { grep -q "length comparison to zero" <<<"$1" && grep -q "clippy::len_zero" <<<"$1"; }

echo "== the bundled shell's Rust toolchain"
project "$T/a"
cd "$T/a" || exit 2
out=$("$R" clippy 2>&1); rc=$?
[ "$rc" = 0 ] && linted "$out"; check $? "the pinned default toolchain provides Clippy" "$out"
grep -q '^version = ' Cargo.lock 2>/dev/null || cargo generate-lockfile --offline -q
echo "// changed" >>src/main.rs
out=$("$R" clippy --locked 2>&1); rc=$?
[ "$rc" = 0 ] && linted "$out"; check $? "rargo clippy --locked reports the lint" "$out"

echo "== two worktrees select the same minimal toolchain"
toolchain=$(printf '[toolchain]\nchannel = "%s"\nprofile = "minimal"\n' "$FILE_TC")
project "$T/c" "$toolchain"
project "$T/d" "$toolchain"
(cd "$T/c" && "$R" clippy >"$E/c.out" 2>&1; echo $? >"$E/c.rc") &
(cd "$T/d" && "$R" clippy >"$E/d.out" 2>&1; echo $? >"$E/d.rc") &
wait
for w in c d; do
    out=$(cat "$E/$w.out")
    [ "$(cat "$E/$w.rc")" = 0 ] && linted "$out"; check $? "worktree $w reports Clippy's lint for a minimal profile" "$out"
done

echo "== a build script that runs rustfmt"
script_project() {
    mkdir -p "$1/src"
    (
        cd "$1" || exit 1
        git init -q .
        printf '[package]\nname = "fmtme"\nversion = "0.1.0"\nedition = "2021"\n' >Cargo.toml
        printf '%s' "$2" >build.rs
        printf 'fn main() {\n    println!("{}", env!("FMT_SAID"));\n}\n' >src/main.rs
        git add -A
        git -c user.name=t -c user.email=t@t.invalid commit -qm one
    )
}
script_project "$T/f" '
use std::process::Command;
fn main() {
    let o = Command::new("rustfmt").arg("--version").output().expect("running rustfmt");
    if !o.status.success() {
        panic!("rustfmt failed: {}", String::from_utf8_lossy(&o.stderr));
    }
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(said.starts_with("rustfmt "));
    let rustc = Command::new("rustc").arg("--version").output().unwrap();
    assert!(rustc.status.success());
    println!("cargo:rustc-env=FMT_SAID=rustfmt with {}", String::from_utf8_lossy(&rustc.stdout).trim());
}
'
cd "$T/f" || exit 2
printf '%s\n' "$toolchain" >rust-toolchain.toml
out=$("$R" run -q 2>&1); rc=$?
[ "$rc" = 0 ] && [[ "$out" == "rustfmt with rustc $FILE_TC "* ]]
check $? "the build script runs rustfmt with the project's Rust toolchain" "$out"

echo "== an unavailable toolchain component"
printf '%s\ncomponents = ["no-such-component"]\n' "$toolchain" >rust-toolchain.toml
out=$("$R" build 2>&1); rc=$?
[ "$rc" -ne 0 ] && grep -q 'no-such-component' <<<"$out"
check $? "the flake diagnostic names the unavailable component" "$out"

cd /
echo "$((checks - fails)) passed, $fails failed"
[ $fails = 0 ]
