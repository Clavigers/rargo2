#!/bin/bash
# End-to-end checks of what workers do with incrementally compiled crates:
# a workspace crate compiled on a worker leaves its incremental state there,
# the state is used in place or comes home, and losing the worker loses
# nothing but time.
#
#   dev/e2e-worker.sh
#
# A server with one core and a worker with four, then with one, in network
# namespaces (dev/netns.sh). Needs root. Its workspace and client cache are
# in RARGO_E2E_WORKER (default $RARGO_WORK/e2e-worker), which it empties.
set -u
work=${RARGO_WORK:-/root/rargo-work}
here=$(cd "$(dirname "$0")" && pwd)
export RARGO_BIN=${RARGO_BIN:-$(cd "$here/.." && pwd)/target/release}
E=${RARGO_E2E_WORKER:-$work/e2e-worker}
# The rig's directory (dev/netns.sh): RIG_STATE under $RARGO_WORK, or as it is if absolute.
case ${RIG_STATE:-nstest} in /*) rig=$RIG_STATE ;; *) rig=$work/${RIG_STATE:-nstest} ;; esac
proj=$E/ws
export RARGO_HIDE=$E
# Off unless asked for (D65); the suite checks it.
export RARGO_STATE_AHEAD=1
# An idle local core does not also run a compile the worker was given.
# These crates are small, so that race would finish here and the state
# the checks look for would never be left on the worker.
export RARGO_HEDGE=0
export RARGO_CACHE_DIR=$E/client-cache
pass=0; fail=0
ok() { pass=$((pass + 1)); echo "ok    $1"; }
bad() { fail=$((fail + 1)); echo "FAIL  $1"; [ -n "${2:-}" ] && echo "$2" | tail -15 | sed 's/^/        /'; }
check() { if [ "$1" = 0 ]; then ok "$2"; else bad "$2" "${3:-}"; fi; }
R() { nsenter --net=/run/netns/${RIG:-rargo}-cl "$RARGO_BIN/rargo" "$@"; }
# A figure from the workers part of the status text: the number before PATTERN.
stat() { R --rargo-status | tr '\n' ' ' | grep -oE "[0-9.]+ $1" | head -1 | cut -d' ' -f1; }
# Bytes in the worker's store, 0 when it has none yet.
store_bytes() { du -sb "$rig/w1/store" 2>/dev/null | awk '{print $1; found=1} END{if (!found) print 0}'; }
# Returns when pushes are acknowledged, states sent ahead have been sent,
# and incremental state that is due has come home.
settle() { R --rargo-settle >/dev/null; }
# A rargod whose command names this state directory (not the scope that started it).
rargod_pids() { # state-dir
    local pid comm
    for pid in $(pgrep -f "rargod --state $1 " || true); do
        comm=$(ps -p "$pid" -o comm= 2>/dev/null || true)
        [ "$comm" = rargod ] && echo "$pid"
    done
}
# The worker is stopped. It tells the server it is leaving, and this
# returns when the server has dropped it.
stop_worker() {
    local before srv pid
    before=$(grep -c "is gone" "$rig/srv.log" 2>/dev/null || true)
    srv=$(rargod_pids "$rig/srv" | head -1)
    for pid in $(rargod_pids "$rig/w1"); do
        kill -TERM "$pid" 2>/dev/null || true
        tail --pid="$pid" -f /dev/null
    done
    tail -n +1 -f --pid="$srv" "$rig/srv.log" | awk -v need="$((${before:-0} + 1))" '
        /is gone/ { c++; if (c >= need) { found = 1; exit } }
        END { exit found ? 0 : 1 }
    '
}
wait_worker_gone() { stop_worker; }
# The worker keeps its store and advertises it, and is given no core, so
# the next compile runs on the server and fetches the state.
worker_holds_only() {
    local joined srv wpid
    stop_worker
    joined=$(grep -c "joined" "$rig/srv.log" 2>/dev/null || true)
    srv=$(rargod_pids "$rig/srv" | head -1)
    nohup systemd-run --scope -q -p CPUQuota=100% -p MemoryMax=30G -p Delegate=yes \
        ip netns exec "${RIG}-w1" "$RARGO_BIN/rargod" --state "$rig/w1" --worker 10.77.0.1:7711 \
        --worker-key "$rig/link.key" --jobs 0 >>"$rig/w1.log" 2>&1 &
    wpid=$!
    tail -n +1 -f --pid="$srv" "$rig/srv.log" | awk -v need="$((${joined:-0} + 1))" '
        /joined/ { c++; if (c >= need) { found = 1; exit } }
        END { exit found ? 0 : 1 }
    '
    # The scope lives as long as the worker. Record it so a later signal
    # still names a process if the worker's own pid is the scope.
    echo "$wpid" >"$rig/w1.scope"
}

# The two scenarios share nothing, so they run at the same time, each
# in a rig of its own. A phase name in the environment runs just that one.
if [ -z "${E2E_WORKER_PHASE:-}" ]; then
    base=$E
    rm -rf "$base"
    mkdir -p "$base"
    E2E_WORKER_PHASE=wide RIG="${RIG:-rargo}a" RIG_STATE="$rig-wide" RARGO_E2E_WORKER="$base/wide" \
        "$0" >"$base/wide.log" 2>&1 &
    wide=$!
    E2E_WORKER_PHASE=one RIG="${RIG:-rargo}b" RIG_STATE="$rig-one" RARGO_E2E_WORKER="$base/one" \
        "$0" >"$base/one.log" 2>&1 &
    one=$!
    wrc=0; orc=0
    wait "$wide" || wrc=$?
    wait "$one" || orc=$?
    echo "== wide worker"
    cat "$base/wide.log"
    echo "== one-core worker"
    cat "$base/one.log"
    awk '
        /^[0-9]+ passed, [0-9]+ failed$/ { p += $1; f += $3; n++ }
        END {
            if (n < 2) { print "0 passed, 1 failed"; exit 1 }
            printf "%d passed, %d failed\n", p, f
            exit (f == 0 ? 0 : 1)
        }
    ' "$base/wide.log" "$base/one.log"
    rc=$?
    rm -rf "$base"
    [ "$wrc" = 0 ] && [ "$orc" = 0 ] && [ "$rc" = 0 ]
    exit
fi

# A library crate.
leaf() { # name value
    mkdir -p "$proj/$1/src"
    printf '[package]\nname = "%s"\nversion = "0.1.0"\nedition = "2021"\n' "$1" >"$proj/$1/Cargo.toml"
    {
        echo "pub fn value() -> u64 { $2 }"
        echo '// This is set by the selected Nix shell on every compiler action.'
        echo 'const _: &str = env!("NIX_CC");'
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

first="la lb lc ld"
warm="lk ll lm ln"
second="le lf lg lh li lj"
if [ "$E2E_WORKER_PHASE" != one ]; then
"$here/netns.sh" down >/dev/null 2>&1
rm -rf "$E"; mkdir -p "$proj"
for l in $first; do leaf $l 1; done
app $first
(cd "$proj" && git init -q . && git add -A && git -c user.name=t -c user.email=t@example.invalid commit -qm one)
"$here/netns.sh" up 1 4 1 1 0 1gbit >/dev/null 2>&1
. "$rig/client.env"
cd "$proj" || exit 2

# The first build teaches the server what a compile reads; the second has
# compiles to give away and sends the worker the toolchain.
out=$(R build 2>&1); check $? "a workspace builds on a server with a worker" "$out"
out=$(R run -q 2>&1); [ "$out" = "total 4" ]; check $? "its program runs (total 4)" "$out"
for l in $warm; do leaf $l 0; done
app $first $warm
before=$(store_bytes)
out=$(R build 2>&1); check $? "the workspace builds after four crates were added" "$out"
# The toolchain has to have crossed the link before the worker is of use.
settle
gain=$(( $(store_bytes) - before ))
# A toolchain is several megabytes; the incremental state of these crates is not.
[ "$gain" -gt 8388608 ]; check $? "the toolchain is on the worker (+$((gain / 1048576)) MiB)" "$(R --rargo-status)"

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
settle
homed=$(stat 'files came home')
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
settle
ahead=$(stat 'states made here sent to workers ahead')
[ "${ahead:-0}" -ge 1 ]; check $? "states made on the server were sent to the worker ahead of need ($ahead)" "$(R --rargo-status)"
wait_worker_gone
for l in $second; do sed -i '1s/.*/pub fn value() -> u64 { 3 }/' "$proj/$l/src/lib.rs"; done
out=$(R build 2>&1); check $? "the workspace builds after the worker was killed" "$out"
out=$(R run -q 2>&1); [ "$out" = "total 22" ]; check $? "its program shows the last edit (total 22)" "$out"
R --rargo-status | grep -A12 '^workers' | tr -s ' ' | sed 's/^/        /'
R --rargo-status | grep '^incremental' | tr -s ' ' | sed 's/^/        /'
grep -c "panicked" "$rig/srv.log" | { read n; [ "$n" = 0 ]; check $? "the server did not panic"; }
cd /
"$here/netns.sh" down >/dev/null 2>&1
umount -l "$rig/srv/mnt" "$rig/w1/mnt" 2>/dev/null || true
rm -rf "$E"
echo
echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
exit
fi

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
. "$rig/client.env"
cd "$proj" || exit 2
out=$(R build 2>&1); check $? "one-core worker: a workspace builds" "$out"
for l in $warm; do leaf $l 0; done
app $first $warm
before=$(store_bytes)
out=$(R build 2>&1); check $? "one-core worker: it builds after four crates were added" "$out"
# The toolchain has to have crossed the link before the worker is of use.
settle
gain=$(( $(store_bytes) - before ))
[ "$gain" -gt 8388608 ]; check $? "one-core worker: the toolchain is on the worker (+$((gain / 1048576)) MiB)" "$(R --rargo-status)"
for l in $second; do leaf $l 10; done
app $first $warm $second
out=$(R build 2>&1); check $? "one-core worker: it builds after six crates were added" "$out"
left=$(stat 'files, of which')
settle
homed=$(stat 'files came home')
[ "${left:-0}" -ge 2 ] && [ "${homed:-1}" = 0 ]; check $? "states stay on the worker while the background fetch is put off ($left files left, $homed home)" "$(R --rargo-status)"
for l in $second; do sed -i '1s/.*/pub fn value() -> u64 { 100 }/' "$proj/$l/src/lib.rs"; done
out=$(R build 2>&1); check $? "one-core worker: it builds after an edit to six crates" "$out"
out=$(R run -q 2>&1); [ "$out" = "total 604" ]; check $? "its program shows the edit (total 604)" "$out"
none=$(R --rargo-status | grep '^incremental' | grep -oE '[0-9]+ none' | cut -d' ' -f1)
# Make sure some newest states are on the worker only. Restart it with
# no core, so the one compile that follows runs here and fetches them.
for l in $second; do sed -i '1s/.*/pub fn value() -> u64 { 5 }/' "$proj/$l/src/lib.rs"; done
out=$(R build 2>&1); check $? "one-core worker: it builds after a second edit" "$out"
worker_holds_only
for l in $second; do sed -i '1s/.*/pub fn value() -> u64 { 6 }/' "$proj/$l/src/lib.rs"; done
out=$(R build 2>&1); check $? "one-core worker: it builds while the worker has no core" "$out"
needed=$(stat 'files were fetched for a compile here')
[ "${needed:-0}" -ge 1 ]; check $? "a compile on the server fetched the state it started from ($needed files)" "$(R --rargo-status)"
wait_worker_gone
for l in $second; do sed -i '1s/.*/pub fn value() -> u64 { 7 }/' "$proj/$l/src/lib.rs"; done
out=$(R build 2>&1); check $? "it builds after the worker died with states on it" "$out"
out=$(R run -q 2>&1); [ "$out" = "total 46" ]; check $? "its program shows the last edit (total 46)" "$out"
R --rargo-status | grep -A12 '^workers' | tr -s ' ' | sed 's/^/        /'
R --rargo-status | grep '^incremental' | tr -s ' ' | sed 's/^/        /'
grep -c "panicked" "$rig/srv.log" | { read n; [ "$n" = 0 ]; check $? "the server did not panic"; }

cd /
"$here/netns.sh" down >/dev/null 2>&1
umount -l "$rig/srv/mnt" "$rig/w1/mnt" 2>/dev/null
rm -rf "$E"
echo
echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
