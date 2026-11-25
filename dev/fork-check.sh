#!/bin/bash
set -u
work=${RARGO_WORK:-/root/rargo-work}
. "$work/env.sh"
export RUSTUP_TOOLCHAIN=${RUSTUP_TOOLCHAIN:-rargo}
T=${RARGO_FORK_CHECK:-$work/fork-check}
rm -rf "$T"; mkdir -p "$T"; cd "$T"
fails=0; checks=0
ok() { checks=$((checks + 1)); echo "  ok   $1"; }
bad() { checks=$((checks + 1)); fails=$((fails + 1)); echo "  FAIL $1"; [ -n "${2:-}" ] && echo "$2" | sed 's/^/       | /' | head -30; }
svh() { python3 -c "import sys;d=open(sys.argv[1],'rb').read();print(d[24:40].hex())" "$1"; }
lib() {
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
locs() {
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
pm() {
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

echo "== function slots"
mkdir -p slots && cd slots
ulimit -c 0
cat >prog.rs <<'EOF'
use std::hint::black_box;
unsafe extern "C" {
    fn fork() -> i32;
    fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    fn _exit(code: i32) -> !;
}
#[inline(always)]
fn inlined(x: u32) -> u32 { x * 3 }
#[inline(never)]
fn generic<T: Default + Copy>(t: T) -> T { black_box(t); T::default() }
#[inline(never)]
fn in_thread() -> u32 { black_box(5) }
#[inline(never)]
fn in_child() -> u32 { black_box(6) }
#[inline(never)]
fn before_abort() { black_box(()); }
#[inline(never)]
fn second_run() -> u32 { black_box(8) }
#[inline(never)]
fn never_called() -> u32 { black_box(9) }
fn main() {
    black_box(never_called as fn() -> u32);
    if std::env::args().nth(1).as_deref() == Some("again") {
        black_box(second_run());
        return;
    }
    black_box(inlined(black_box(2)));
    black_box(generic(1u32));
    black_box(generic(2u64));
    let add = |x: u32| x + black_box(1);
    black_box(add(1));
    std::thread::spawn(|| black_box(in_thread())).join().unwrap();
    unsafe {
        let pid = fork();
        if pid == 0 {
            black_box(in_child());
            _exit(0);
        }
        let mut status = 0;
        waitpid(pid, &mut status, 0);
    }
    before_abort();
    std::process::abort();
}
EOF
cat >marks.py <<'EOF'
import sys
exe, marks = sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else None
d = open(exe, 'rb').read()
u = lambda at, n: int.from_bytes(d[at:at + n], 'little')
shoff, shentsize, shnum, shstrndx = u(0x28, 8), u(0x3a, 2), u(0x3c, 2), u(0x3e, 2)
sec = lambda i: d[shoff + i * shentsize: shoff + (i + 1) * shentsize]
strtab = sec(shstrndx)
names_at = int.from_bytes(strtab[0x18:0x20], 'little')
found = {}
for i in range(shnum):
    h = sec(i)
    n = int.from_bytes(h[0:4], 'little')
    name = d[names_at + n: d.index(b'\0', names_at + n)].decode()
    found[name] = (int.from_bytes(h[0x18:0x20], 'little'), int.from_bytes(h[0x20:0x28], 'little'))
if '.rargo_slots' not in found:
    print('no-slots')
    sys.exit(0)
off, size = found['.rargo_slot_names']
names = d[off:off + size].split(b'\0')[:-1]
slots = found['.rargo_slots'][1]
print('slots', slots, 'names', len(names))
m = open(marks, 'rb').read() if marks else b''
print('file', len(m))
for i, n in enumerate(names):
    print('+' if i < len(m) and m[i] else '-', n.decode())
EOF
rustc --edition=2021 -O -C codegen-units=4 -C symbol-mangling-version=v0 -Zrargo-fn-slots prog.rs -o prog 2>&1 | head -5
rustc --edition=2021 -O prog.rs -o plain 2>&1 | head -5
report=$(python3 -I marks.py prog)
grep -q '^slots \([0-9]*\) names \1$' <<<"$report" && ok "every slot is named, in slot order ($(head -1 <<<"$report" | cut -d' ' -f2) slots)" || bad "every slot is named, in slot order" "$(head -1 <<<"$report")"
[ "$(python3 -I marks.py plain)" = no-slots ] && ok "a build without the flag has no slots" || bad "a build without the flag has no slots"
rm -f marks.bin
env -u RARGO_FN_SLOTS bash -c './prog; true' >/dev/null 2>&1
[ "$(ls)" = "$(printf 'marks.py\nplain\nprog\nprog.rs')" ] && ok "without RARGO_FN_SLOTS the program runs and writes nothing" || bad "without RARGO_FN_SLOTS the program runs and writes nothing" "$(ls)"
RARGO_FN_SLOTS=$PWD/marks.bin bash -c './prog; true' >/dev/null 2>&1
first=$(python3 -I marks.py prog marks.bin)
RARGO_FN_SLOTS=$PWD/marks.bin ./prog again >/dev/null 2>&1
second=$(python3 -I marks.py prog marks.bin)
marked() { grep -cE "^\+ _R[A-Za-z0-9_]*_4prog$2\$" <<<"$1"; }
unmarked() { grep -cE "^- _R[A-Za-z0-9_]*_4prog$2\$" <<<"$1"; }
[ "$(sed -n 2p <<<"$first")" = "file $(head -1 <<<"$report" | cut -d' ' -f2)" ] && ok "the marks file is one byte per slot" || bad "the marks file is one byte per slot" "$(sed -n 2p <<<"$first")"
for f in 4main 7inlined 9in_thread 8in_child 12before_abort; do
    [ "$(marked "$first" "$f")" = 1 ] && ok "marked: $f" || bad "marked: $f" "$(grep "$f" <<<"$first")"
done
[ "$(marked "$first" "7generic[my]EB2_")" = 2 ] && ok "marked: generic at u32 and u64, one slot each" || bad "marked: generic at u32 and u64, one slot each" "$(grep generic <<<"$first")"
[ "$(marked "$first" "4main(s_)?0B3_")" = 2 ] && ok "marked: both closures in main" || bad "marked: both closures in main" "$(grep 4prog4main <<<"$first")"
[ "$(unmarked "$first" "12never_called")" = 1 ] && ok "unmarked: a function never called" || bad "unmarked: a function never called" "$(grep never_called <<<"$first")"
[ "$(unmarked "$first" "10second_run")" = 1 ] && [ "$(marked "$second" "10second_run")" = 1 ] && [ "$(marked "$second" "8in_child")" = 1 ] && ok "a second run of the binary marks the same file" || bad "a second run of the binary marks the same file" "$(grep -E 'second_run|in_child' <<<"$second")"
[ "$(grep -c '^+' <<<"$second")" -ge "$(grep -c '^+' <<<"$first")" ] && ok "marks are never cleared" || bad "marks are never cleared"
mir_calls() { rustc --edition=2021 -O "$@" --emit=mir prog.rs -o - 2>/dev/null | sed -n '/^fn main()/,/^}/p' | grep -c 'inlined('; }
[ "$(mir_calls)" = 0 ] && [ "$(mir_calls -Zrargo-fn-slots)" = 1 ] && ok "the MIR inliner keeps a function its own slot: main calls inlined() only with the flag" || bad "the MIR inliner keeps a function its own slot" "without: $(mir_calls), with: $(mir_calls -Zrargo-fn-slots)"
cat >plugin.rs <<'EOF'
#[unsafe(no_mangle)]
pub extern "C" fn plugin_work(x: u32) -> u32 { std::hint::black_box(x).wrapping_mul(7) }
EOF
cat >host.rs <<'EOF'
use std::hint::black_box;
unsafe extern "C" {
    fn dlopen(path: *const std::ffi::c_char, flags: i32) -> *mut u8;
    fn dlsym(handle: *mut u8, name: *const std::ffi::c_char) -> *mut u8;
}
#[inline(never)]
fn after_plugin() -> u32 { black_box(3) }
fn main() {
    let path = std::ffi::CString::new(std::env::args().nth(1).unwrap()).unwrap();
    let work: extern "C" fn(u32) -> u32 = unsafe {
        let h = dlopen(path.as_ptr(), 2);
        assert!(!h.is_null());
        std::mem::transmute(dlsym(h, c"plugin_work".as_ptr()))
    };
    for i in 0..1000 { black_box(work(i)); }
    black_box(after_plugin());
}
EOF
rustc --edition=2021 -O -C symbol-mangling-version=v0 -Zrargo-fn-slots --crate-type cdylib plugin.rs -o libplugin.so 2>&1 | head -5
rustc --edition=2021 -O -C symbol-mangling-version=v0 -Zrargo-fn-slots host.rs -o host 2>&1 | head -5
rm -f host.bin
RARGO_FN_SLOTS=$PWD/host.bin ./host $PWD/libplugin.so >/dev/null 2>&1; rc=$?
hosted=$(python3 -I marks.py host host.bin)
[ $rc = 0 ] && [ "$(grep -cE '^\+ _R[A-Za-z0-9_]*_4host12after_plugin$' <<<"$hosted")" = 1 ] && ok "a cdylib with its own std, loaded into a marked process, leaves the process's marks alone" || bad "a cdylib with its own std, loaded into a marked process, leaves the process's marks alone" "exit $rc; $(grep after_plugin <<<"$hosted")"
cd "$T"

echo "fork-check: $((checks - fails)) of $checks passed"
exit $fails
