#!/bin/bash
# Checks of rargo's rustc fork (toolchain `rargo`) on small crates, with
# rustc invoked directly. What is checked is what rargo relies on:
#   1. edits that keep the interface and the line numbers keep the crate hash
#   2. edits that change line numbers or the interface change it
#   3. a crate that was not rebuilt after such an edit to its dependency
#      still gives correct positions to the crates that use both
#   4. incremental rlibs are reproducible
#   5. -Zno-codegen gives the full build's metadata, on stable
set -u
work=${RARGO_WORK:-/root/rargo-work}
. "$work/env.sh"
export RUSTUP_TOOLCHAIN=rargo
T=$work/fork-check
rm -rf "$T"; mkdir -p "$T"; cd "$T"
fails=0; checks=0
ok() { checks=$((checks + 1)); echo "  ok   $1"; }
bad() { checks=$((checks + 1)); fails=$((fails + 1)); echo "  FAIL $1"; [ -n "${2:-}" ] && echo "$2" | sed 's/^/       | /' | head -30; }
svh() { python3 -c "import sys;d=open(sys.argv[1],'rb').read();print(d[24:40].hex())" "$1"; }
lib() { # name src outdir [extra...]: compiles src as <name>/lib.rs (the
    # file name is part of the crate hash, so every variant uses the same one)
    local name=$1 src=$2 out=$3; shift 3
    mkdir -p "$out" "$name"
    [ "$src" = "$name/lib.rs" ] || cp "$src" "$name/lib.rs"
    rustc --crate-name "$name" --edition=2021 --crate-type lib --emit=metadata,link -C metadata=x \
        -C debuginfo=2 -L "dependency=$T/deps" "$@" "$name/lib.rs" --out-dir "$out"
}

mkdir -p b
cat >b/lib.rs <<'EOF'
pub struct Thing { pub x: u32 }
pub fn first(v: u32) -> u32 {
    let y = v + 1;
    y * 2
}
#[macro_export]
macro_rules! must_be_u32 { ($e:expr) => { $crate::takes_u32($e) } }
pub fn takes_u32(v: u32) -> u32 { v }
pub fn generic<T: Copy>(t: T) -> T { t }
#[macro_export]
macro_rules! assert_four { ($t:ty) => { const { assert!(::core::mem::size_of::<$t>() == 4, "not four bytes") } } }
EOF
cp b/lib.rs b/orig.rs
lib b b/orig.rs b0 || exit 2
h0=$(svh b0/libb.rmeta)

echo "== crate hash"
sed 's/let y = v + 1;/let y = v + 123456;/' b/orig.rs >b/longer.rs
lib b b/longer.rs b1
[ "$(svh b1/libb.rmeta)" = "$h0" ] && ok "a longer line inside a body keeps the crate hash" || bad "a longer line inside a body keeps the crate hash"
cmp -s b0/libb.rmeta b1/libb.rmeta && bad "the metadata bytes differ (otherwise this checks nothing)" || ok "the metadata bytes differ (the hash is what stays)"
sed 's/    y \* 2/    let z = y;\n    z * 2/' b/orig.rs >b/morelines.rs
lib b b/morelines.rs b2
[ "$(svh b2/libb.rmeta)" != "$h0" ] && ok "an added line inside a body changes it (later lines moved)" || bad "an added line inside a body changes it"
sed 's/pub fn first(v: u32)/pub fn first(v: u64)/; s/let y = v + 1;/let y = v as u32 + 1;/' b/orig.rs >b/api.rs
lib b b/api.rs b3
[ "$(svh b3/libb.rmeta)" != "$h0" ] && ok "a signature change changes it" || bad "a signature change changes it"

echo "== a crate not rebuilt after its dependency changed bytes but not hash"
# a's inlinable functions expand b's macro: a's metadata holds spans into
# b's file. c is compiled against a as it was built over the old b, and
# against a rebuilt over the new b; the positions c sees must be the same.
mkdir -p a c
cat >a/lib.rs <<'EOF'
#[inline]
pub fn uses() -> u32 { b::must_be_u32!(7) + b::first(1) }
pub fn generic_user<T: Copy>(t: T) -> T { b::generic(t) }
pub fn checked<T>() { b::assert_four!(T) }
EOF
cat >c/lib.rs <<'EOF'
pub fn c() -> u32 { a::uses() + a::generic_user(1u32) }
EOF
cp b/orig.rs b/lib.rs
lib a a/lib.rs a_old --extern b=b0/libb.rlib
cp b/longer.rs b/lib.rs
lib a a/lib.rs a_new --extern b=b1/libb.rlib
locs() { # a-dir out-dir
    rustc --crate-name c --edition=2021 --crate-type lib --emit=llvm-ir,link -C debuginfo=2 -C codegen-units=1 \
        -L "dependency=$T/b1" --extern a="$1/liba.rlib" --extern b=b1/libb.rlib c/lib.rs --out-dir "$2" 2>&1 || return 1
    grep -o 'DILocation(line: [0-9]*, column: [0-9]*' "$2/c.ll" | sort | uniq -c
}
old=$(locs a_old c_old); rc_old=$?
new=$(locs a_new c_new); rc_new=$?
[ $rc_old -eq 0 ] && ok "c builds against the old a and the new b" || bad "c builds against the old a and the new b" "$old"
[ $rc_new -eq 0 ] && [ -n "$new" ] && [ "$old" = "$new" ] && ok "c's debug locations are the same either way ($(wc -l <<<"$new") distinct)" || bad "c's debug locations are the same either way" "$(diff <(echo "$old") <(echo "$new"))"
out=$(rustc --crate-name c --edition=2021 --crate-type lib --emit=metadata -L "dependency=$T/b1" \
      --extern a=a_old/liba.rmeta --extern b=b1/libb.rmeta - --out-dir c_err 2>&1 <<'EOF'
pub fn bad() -> u32 { b::takes_u32(a::uses() as u64) }
EOF
)
grep -q "panicked\|internal compiler error" <<<"$out" && bad "an error involving both crates is reported without an internal error" "$out" || ok "an error involving both crates is reported without an internal error"
grep -q 'b/lib.rs:8' <<<"$out" && ok "and points at b's function on its real line" || bad "and points at b's function on its real line" "$out"
# A compile-time assertion inside b's macro, instantiated by c through a's
# generic function: the error is reported at the span in b's file that a's
# metadata recorded.
pm() { # a-dir out-dir
    rustc --crate-name c --edition=2021 --crate-type lib --emit=link -L "dependency=$T/b1" \
        --extern a="$1/liba.rlib" --extern b=b1/libb.rlib - --out-dir "$2" 2>&1 <<'EOF'
pub fn f() { a::checked::<u64>() }
EOF
}
err_old=$(pm a_old pm_old | grep -E '^ *--> |^error')
err_new=$(pm a_new pm_new | grep -E '^ *--> |^error')
grep -q 'b/lib.rs:11' <<<"$err_new" && ok "a post-monomorphization error in b's macro is reported at b/lib.rs:11 (a rebuilt)" || bad "a post-monomorphization error in b's macro is reported at b/lib.rs:11 (a rebuilt)" "$err_new"
[ "$err_old" = "$err_new" ] && ok "and at the same place with a not rebuilt" || bad "and at the same place with a not rebuilt" "$(diff <(echo "$err_old") <(echo "$err_new"))"
cp b/orig.rs b/lib.rs

echo "== reproducible incremental rlibs"
cp b/morelines.rs b/lib.rs
for i in 1 2; do lib b b/lib.rs r$i -C incremental=inc$i -C codegen-units=4; done
cmp -s r1/libb.rlib r2/libb.rlib && ok "two incremental builds from scratch give the same rlib" || bad "two incremental builds from scratch give the same rlib" "$(ar t r1/libb.rlib; ar t r2/libb.rlib)"

echo "== metadata without code generation"
lib b b/lib.rs m1
lib b b/lib.rs m2 -Zno-codegen
cmp -s m1/libb.rmeta m2/libb.rmeta && ok "-Zno-codegen gives the full build's metadata byte for byte" || bad "-Zno-codegen gives the full build's metadata byte for byte"
out=$(rustc -Zthis-is-not-allowed lib.rs 2>&1); grep -q "only accepted on the nightly" <<<"$out" && ok "other -Z flags are still refused on stable" || bad "other -Z flags are still refused on stable" "$out"

echo "fork-check: $((checks - fails)) of $checks passed"
exit $fails
