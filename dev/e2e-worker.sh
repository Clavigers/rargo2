#!/bin/bash
# End-to-end checks of what workers do with incrementally compiled crates:
# a workspace crate compiled on a worker leaves its incremental state there,
# the state is used in place or comes home, and losing the worker loses
# nothing but time.
#
#   dev/e2e-worker.sh
#
# A server with one core and a worker with four, then with one, in network
# namespaces (dev/netns.sh). Needs root.
set -u
work=${RARGO_WORK:-/root/rargo-work}
here=$(cd "$(dirname "$0")" && pwd)
export RARGO_BIN=${RARGO_BIN:-$(cd "$here/.." && pwd)/target/release}
proj=$work/e2e-worker/ws
export RARGO_HIDE=$work/e2e-worker
export RARGO_CACHE_DIR=$work/e2e-worker/client-cache
pass=0; fail=0
ok() { pass=$((pass + 1)); echo "ok    $1"; }
bad() { fail=$((fail + 1)); echo "FAIL  $1"; [ -n "${2:-}" ] && echo "$2" | tail -15 | sed 's/^/        /'; }
check() { if [ "$1" = 0 ]; then ok "$2"; else bad "$2" "${3:-}"; fi; }
R() { nsenter --net=/run/netns/${RIG:-rargo}-cl "$RARGO_BIN/rargo" "$@"; }
# A figure from the workers part of the status text: the number before PATTERN.
stat() { R --rargo-status | tr '\n' ' ' | grep -oE "[0-9.]+ $1" | head -1 | cut -d' ' -f1; }

# A library crate that takes a compiler a second or two.
leaf() { # name value
    mkdir -p "$proj/$1/src"
    printf '[package]\nname = "%s"\nversion = "0.1.0"\nedition = "2021"\n' "$1" >"$proj/$1/Cargo.toml"
    {
        echo "pub fn value() -> u64 { $2 }"
        for i in $(seq 1 1500); do
            echo "pub fn f$i(v: &[u64]) -> String { let mut s = String::new(); for (i, x) in v.iter().enumerate() { s.push_str(&format!(\"{}:{}:{} \", i, x, $i)); } s }"
        done
    } >"$proj/$1/src/lib.rs"
}
app() { # leaf names...
    mkdir -p "$proj/app/src"
    {
        printf '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\n'
        for l in "$@"; do echo "$l = { path = \"../$l\" }"; done
    } >"$proj/app/Cargo.toml"
    {
        echo "fn main() {"
        echo "    let mut total = 0u64;"
        for l in "$@"; do echo "    total += $l::value();"; done
        echo '    println!("total {total}");'
        echo "}"
    } >"$proj/app/src/main.rs"
    {
        echo "[workspace]"
        echo 'resolver = "2"'
        printf 'members = ["app"'
        for l in "$@"; do printf ', "%s"' "$l"; done
        echo "]"
    } >"$proj/Cargo.toml"
}

"$here/netns.sh" down >/dev/null 2>&1
rm -rf "$work/e2e-worker"; mkdir -p "$proj"
first="la lb lc ld"
warm="lk ll lm ln"
second="le lf lg lh li lj"
for l in $first; do leaf $l 1; done
app $first
(cd "$proj" && git init -q . && git add -A && git -c user.name=t -c user.email=t@example.invalid commit -qm one)
"$here/netns.sh" up 1 4 1 1 0 1gbit >/dev/null 2>&1
. "$work/${RIG_STATE:-nstest}/client.env"
cd "$proj" || exit 2

# The first build teaches the server what a compile reads; the second has
# compiles to give away and sends the worker the toolchain.
out=$(R build 2>&1); check $? "a workspace builds on a server with a worker" "$out"
out=$(R run -q 2>&1); [ "$out" = "total 4" ]; check $? "its program runs (total 4)" "$out"
for l in $warm; do leaf $l 0; done
app $first $warm
out=$(R build 2>&1); check $? "the workspace builds after four crates were added" "$out"
# The toolchain has to have crossed the link before the worker is of use.
for i in $(seq 1 150); do
    sent=$(stat 'MiB, not sent because')
    [ "${sent%.*}" -ge 300 ] && break
    sleep 0.2
done
sleep 6

# Six new crates at once, one server core: first compiles go to the worker.
for l in $second; do leaf $l 10; done
app $first $warm $second
out=$(R build 2>&1); check $? "the workspace builds after six crates were added" "$out"
out=$(R run -q 2>&1); [ "$out" = "total 64" ]; check $? "its program runs (total 64)" "$out"
unseeded=$(stat 'without state')
left=$(stat 'files, of which')
[ "${unseeded:-0}" -ge 1 ]; check $? "first compiles of workspace crates ran on the worker ($unseeded)" "$(R --rargo-status)"
[ "${left:-0}" -ge 1 ]; check $? "their incremental state was left on the worker ($left files)" "$(R --rargo-status)"

# With the server idle the state comes home by itself.
for i in $(seq 1 150); do
    homed=$(stat 'files came home')
    [ "${homed:-0}" -ge "${left:-1}" ] && break
    sleep 0.2
done
[ "${homed:-0}" -ge "${left:-1}" ]; check $? "the state came home in the background ($homed of $left files)" "$(R --rargo-status)"

# Edit every new crate: each compile starts from a state. Those that go to
# the worker find it there (the worker made it).
before=$(stat 'with the state the worker held')
for l in $second; do sed -i '1s/.*/pub fn value() -> u64 { 100 }/' "$proj/$l/src/lib.rs"; done
out=$(R build 2>&1); check $? "the workspace builds after an edit to six crates" "$out"
out=$(R run -q 2>&1); [ "$out" = "total 604" ]; check $? "its program shows the edit (total 604)" "$out"
after=$(stat 'with the state the worker held')
[ "${after:-0}" -gt "${before:-0}" ]; check $? "edited crates were compiled on the worker from the state it held ($before -> $after)" "$(R --rargo-status)"
none=$(R --rargo-status | grep '^incremental' | grep -oE '[0-9]+ none' | cut -d' ' -f1)

# Edit again and at once, while the latest states are still on the worker:
# a compile that runs on the server fetches what it needs.
for l in $second; do sed -i '1s/.*/pub fn value() -> u64 { 1000 }/' "$proj/$l/src/lib.rs"; done
out=$(R build 2>&1); check $? "the workspace builds after a second edit made at once" "$out"
out=$(R run -q 2>&1); [ "$out" = "total 6004" ]; check $? "its program shows the edit (total 6004)" "$out"
none2=$(R --rargo-status | grep '^incremental' | grep -oE '[0-9]+ none' | cut -d' ' -f1)
[ "$none" = "$none2" ]; check $? "no compile after an edit started without a state ($none, $none2)" "$(R --rargo-status)"

# The worker disappears right after a build that left state on it. Nothing
# but time may be lost.
for l in $second; do sed -i '1s/.*/pub fn value() -> u64 { 2 }/' "$proj/$l/src/lib.rs"; done
out=$(R build 2>&1); check $? "the workspace builds after a third edit" "$out"
ahead=0
for _ in $(seq 1 40); do
    ahead=$(stat 'states made here sent to workers ahead')
    [ "${ahead:-0}" -ge 1 ] && break
    sleep 0.25
done
[ "${ahead:-0}" -ge 1 ]; check $? "states made on the server were sent to the worker ahead of need ($ahead)" "$(R --rargo-status)"
pkill -KILL -f "rargod --state $work/${RIG_STATE:-nstest}/w1" 2>/dev/null
for l in $second; do sed -i '1s/.*/pub fn value() -> u64 { 3 }/' "$proj/$l/src/lib.rs"; done
out=$(R build 2>&1); check $? "the workspace builds after the worker was killed" "$out"
out=$(R run -q 2>&1); [ "$out" = "total 22" ]; check $? "its program shows the last edit (total 22)" "$out"
R --rargo-status | grep -A12 '^workers' | tr -s ' ' | sed 's/^/        /'
R --rargo-status | grep '^incremental' | tr -s ' ' | sed 's/^/        /'
grep -c "panicked" "$work/${RIG_STATE:-nstest}/srv.log" | { read n; [ "$n" = 0 ]; check $? "the server did not panic"; }


# The same with a worker of one core and states that stay on it (the
# background fetch is put off for the length of the test): compiles that
# run on the server have to fetch the state they start from, and when the
# worker dies the states on it are gone.
cd /
"$here/netns.sh" down >/dev/null 2>&1
rm -rf "$proj" "$RARGO_CACHE_DIR"; mkdir -p "$proj"
for l in $first; do leaf $l 1; done
app $first
(cd "$proj" && git init -q . && git add -A && git -c user.name=t -c user.email=t@example.invalid commit -qm one)
RARGO_STATE_HOME_AFTER_MS=3600000 "$here/netns.sh" up 1 1 1 1 0 1gbit >/dev/null 2>&1
. "$work/${RIG_STATE:-nstest}/client.env"
cd "$proj" || exit 2
out=$(R build 2>&1); check $? "one-core worker: a workspace builds" "$out"
for l in $warm; do leaf $l 0; done
app $first $warm
out=$(R build 2>&1); check $? "one-core worker: it builds after four crates were added" "$out"
for i in $(seq 1 150); do
    sent=$(stat 'MiB, not sent because')
    [ "${sent%.*}" -ge 300 ] && break
    sleep 0.2
done
sleep 6
for l in $second; do leaf $l 10; done
app $first $warm $second
out=$(R build 2>&1); check $? "one-core worker: it builds after six crates were added" "$out"
left=$(stat 'files, of which')
homed=$(stat 'files came home')
[ "${left:-0}" -ge 2 ] && [ "${homed:-1}" = 0 ]; check $? "states stay on the worker while the background fetch is put off ($left files left, $homed home)" "$(R --rargo-status)"
for l in $second; do sed -i '1s/.*/pub fn value() -> u64 { 100 }/' "$proj/$l/src/lib.rs"; done
out=$(R build 2>&1); check $? "one-core worker: it builds after an edit to six crates" "$out"
out=$(R run -q 2>&1); [ "$out" = "total 604" ]; check $? "its program shows the edit (total 604)" "$out"
none=$(R --rargo-status | grep '^incremental' | grep -oE '[0-9]+ none' | cut -d' ' -f1)
# Make sure some newest states are on the worker only, then lose it.
for l in $second; do sed -i '1s/.*/pub fn value() -> u64 { 5 }/' "$proj/$l/src/lib.rs"; done
out=$(R build 2>&1); check $? "one-core worker: it builds after a second edit" "$out"
for n in 20 30 40; do
    needed=$(stat 'files were fetched for a compile here')
    [ "${needed:-0}" -ge 1 ] && break
    for l in $second; do sed -i "1s/.*/pub fn value() -> u64 { $n }/" "$proj/$l/src/lib.rs"; done
    out=$(R build 2>&1) || break
done
[ "${needed:-0}" -ge 1 ]; check $? "a compile on the server fetched the state it started from ($needed files)" "$(R --rargo-status)"
pkill -KILL -f "rargod --state $work/${RIG_STATE:-nstest}/w1" 2>/dev/null
sleep 3
for l in $second; do sed -i '1s/.*/pub fn value() -> u64 { 7 }/' "$proj/$l/src/lib.rs"; done
out=$(R build 2>&1); check $? "it builds after the worker died with states on it" "$out"
out=$(R run -q 2>&1); [ "$out" = "total 46" ]; check $? "its program shows the last edit (total 46)" "$out"
R --rargo-status | grep -A12 '^workers' | tr -s ' ' | sed 's/^/        /'
R --rargo-status | grep '^incremental' | tr -s ' ' | sed 's/^/        /'
grep -c "panicked" "$work/${RIG_STATE:-nstest}/srv.log" | { read n; [ "$n" = 0 ]; check $? "the server did not panic"; }

cd /
"$here/netns.sh" down >/dev/null 2>&1
umount -l "$work/${RIG_STATE:-nstest}/srv/mnt" "$work/${RIG_STATE:-nstest}/w1/mnt" 2>/dev/null
rm -rf "$work/e2e-worker"
echo
echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
