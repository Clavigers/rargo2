#!/bin/bash
# End to end: a command that needs a toolchain component the server lacks
# (clippy for `rargo clippy`) gets it installed there on first use.
#
#   dev/e2e-components.sh
#
# The server has toolchains of its own, in $RARGO_COMPONENTS_RUSTUP (default
# $RARGO_WORK/rustup-components; nothing else uses them), installed with the
# minimal profile. Clippy is removed from them before each check, so every
# check starts without it. The first run downloads two minimal toolchains.
# Requires root (FUSE mount, namespaces).
set -u
work=${RARGO_WORK:-/root/rargo-work}
repo=$(cd "$(dirname "$0")/.." && pwd)
. "$work/env.sh"
bin=${RARGO_BIN:-$repo/target/release}
export RARGO_BIN=$bin
R=$bin/rargo
RH=${RARGO_COMPONENTS_RUSTUP:-$work/rustup-components}
host_triple=x86_64-unknown-linux-gnu
DEFAULT_TC=1.87.0 FILE_TC=1.88.0

fails=0
checks=0
ok() { checks=$((checks + 1)); echo "ok    $1"; }
bad() { checks=$((checks + 1)); fails=$((fails + 1)); echo "FAIL  $1"; shift; printf '%s\n' "$@" | sed 's/^/        /' | head -40; }
check() { local rc=$1 name=$2; shift 2; if [ "$rc" = 0 ]; then ok "$name"; else bad "$name" "$@"; fi; }
rh() { RUSTUP_HOME=$RH "$@"; }
has_clippy() { [ -x "$RH/toolchains/$1-$host_triple/bin/cargo-clippy" ] && [ -x "$RH/toolchains/$1-$host_triple/bin/clippy-driver" ]; }
without_clippy() {
    for tc in "$@"; do rh rustup component remove clippy --toolchain "$tc" >/dev/null 2>&1; done
    for tc in "$@"; do has_clippy "$tc" && return 1; done
    return 0
}

echo "== the server's toolchains: $RH"
for tc in $DEFAULT_TC $FILE_TC; do
    if ! rh rustup toolchain list 2>/dev/null | grep -q "^$tc-$host_triple"; then
        out=$(rh rustup toolchain install "$tc" --profile minimal --no-self-update 2>&1) || { echo "cannot install $tc: $out"; exit 1; }
    fi
done
rh rustup default "$DEFAULT_TC" >/dev/null 2>&1
out=$(rh rustup default 2>&1); grep -q "^$DEFAULT_TC-" <<<"$out"; check $? "the server's default toolchain is $DEFAULT_TC" "$out"

RARGO_STATE=$(mktemp -d "$work/e2e-components-state.XXXXXXXX") || exit 1
E=$(mktemp -d "$work/e2e-components.XXXXXXXX") || exit 1
T=$E/client
mkdir -p "$T" "$E/store"
export RARGO_STATE RARGO_HIDE=$T RARGO_CACHE_DIR=$T/.rargo-cache
export RARGO_SERVER_RUSTUP_HOME=$RH RARGO_SERVER_CARGO_HOME=$RARGO_STATE/cargo-home
port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
export RARGO_SERVER=127.0.0.1:$port
RARGO_TOKEN=$("$bin/rargod" token create --state "$RARGO_STATE" --user e2e --admin 2>/dev/null)
export RARGO_TOKEN
# The client's own toolchain choice is not this suite's subject.
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

# A package with one thing clippy objects to (`len_zero`), and nothing
# else to fetch.
project() { # dir [toolchain file contents]
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

echo "== no toolchain file: the server's default, which has no clippy"
project "$T/a"
cd "$T/a" || exit 2
without_clippy $DEFAULT_TC; check $? "clippy is not installed for $DEFAULT_TC on the server"
out=$("$R" clippy 2>&1); rc=$?
[ $rc = 0 ] && linted "$out"; check $? "rargo clippy reports clippy's lint" "$out"
! grep -q "is not installed" <<<"$out"; check $? "and the client sees no error about it" "$out"
has_clippy $DEFAULT_TC; check $? "clippy is now installed for $DEFAULT_TC on the server" "$(ls "$RH/toolchains/$DEFAULT_TC-$host_triple/bin")"
grep -q '^version = ' Cargo.lock 2>/dev/null || cargo generate-lockfile --offline -q
echo "// two" >>src/main.rs
out=$("$R" clippy 2>&1); [ $? = 0 ] && linted "$out"; check $? "the second rargo clippy needs nothing installed" "$out"

echo "== a command that may not fetch still gets the component"
without_clippy $DEFAULT_TC; check $? "clippy is removed again"
echo "// three" >>src/main.rs
out=$("$R" clippy --locked 2>&1); rc=$?
[ $rc = 0 ] && linted "$out"; check $? "rargo clippy --locked reports clippy's lint" "$out"

echo "== a toolchain file that names no components"
project "$T/b" "$(printf '[toolchain]\nchannel = "%s"\nprofile = "minimal"\n' $FILE_TC)"
cd "$T/b" || exit 2
without_clippy $FILE_TC; check $? "clippy is not installed for $FILE_TC on the server"
out=$("$R" clippy 2>&1); rc=$?
[ $rc = 0 ] && linted "$out"; check $? "rargo clippy reports clippy's lint" "$out"
has_clippy $FILE_TC; check $? "clippy is now installed for $FILE_TC, the file's toolchain" "$(ls "$RH/toolchains/$FILE_TC-$host_triple/bin")"

echo "== two at once wait for one installation"
project "$T/c" "$(printf '[toolchain]\nchannel = "%s"\nprofile = "minimal"\n' $FILE_TC)"
project "$T/d" "$(printf '[toolchain]\nchannel = "%s"\nprofile = "minimal"\n' $FILE_TC)"
without_clippy $FILE_TC; check $? "clippy is removed again"
(cd "$T/c" && "$R" clippy >"$E/c.out" 2>&1; echo $? >"$E/c.rc") &
(cd "$T/d" && "$R" clippy >"$E/d.out" 2>&1; echo $? >"$E/d.rc") &
wait
for w in c d; do
    out=$(cat "$E/$w.out")
    [ "$(cat "$E/$w.rc")" = 0 ] && linted "$out"; check $? "worktree $w: rargo clippy reports clippy's lint" "$out"
done

cd /
echo "$((checks - fails)) passed, $fails failed"
[ $fails = 0 ]
