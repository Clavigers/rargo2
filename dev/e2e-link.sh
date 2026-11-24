#!/bin/bash
# End-to-end checks of the client daemon and its link to the server: one
# long-lived session over the UDP transport, changes pushed as they happen,
# survival of an address change, of a daemon's and a server's death, and
# TCP where UDP does not get through.
#
#   dev/e2e-link.sh
#
# Server and client in separate network namespaces (dev/netns.sh), the
# server blind to the client's directories. Needs root.
set -u
work=${RARGO_WORK:-/root/rargo-work}
here=$(cd "$(dirname "$0")" && pwd)
export RARGO_BIN=${RARGO_BIN:-$(cd "$here/.." && pwd)/target/release}
. "$work/env.sh"
E=${RARGO_E2E_LINK:-$work/e2e-link}
proj=$E/ws
export RARGO_HIDE=$E
export RARGO_CACHE_DIR=$E/cache
pass=0; fail=0
ok() { pass=$((pass + 1)); echo "ok    $1"; }
bad() { fail=$((fail + 1)); echo "FAIL  $1"; [ -n "${2:-}" ] && echo "$2" | tail -15 | sed 's/^/        /'; }
check() { if [ "$1" = 0 ]; then ok "$2"; else bad "$2" "${3:-}"; fi; }
R() { nsenter --net=/run/netns/${RIG:-rargo}-cl "$RARGO_BIN/rargo" "$@"; }
dstat() { R --rargo-daemon-status 2>&1; }
# A figure from the server's sync line: the number before PATTERN.
sync_n() { R --rargo-status | grep '^sync' | grep -oE "[0-9]+ $1" | head -1 | cut -d' ' -f1; }
daemons() { pgrep -f "$RARGO_BIN/rargo --rargo-daemon $E" | wc -l; }

"$here/netns.sh" down >/dev/null 2>&1
rm -rf "$E"; mkdir -p "$proj/crates/util/src" "$proj/crates/app/src"
cd "$proj" || exit 2
printf '[workspace]\nresolver = "2"\nmembers = ["crates/*"]\n' >Cargo.toml
printf '[package]\nname = "util"\nversion = "0.1.0"\nedition = "2021"\n' >crates/util/Cargo.toml
echo 'pub fn value() -> i32 { 1 }' >crates/util/src/lib.rs
printf '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\n[dependencies]\nutil = { path = "../util" }\n' >crates/app/Cargo.toml
echo 'fn main() { println!("value {}", util::value()); }' >crates/app/src/main.rs
git init -q . && git add -A && git -c user.name=t -c user.email=t@example.invalid commit -qm one
"$here/netns.sh" up 4 4 0 >/dev/null 2>&1
. "$work/${RIG_STATE:-nstest}/client.env"

echo "== one daemon, one link"
out=$(R run -q 2>&1); [ "$out" = "value 1" ]; check $? "the first command starts a daemon and builds through it" "$out"
[ "$(daemons)" = 1 ]; check $? "one daemon is running ($(daemons))"
st=$(dstat)
grep -q '^link  *udp; 1 made' <<<"$st"; check $? "its link to the server is the UDP transport" "$st"
out=$(R run -q 2>&1); [ "$out" = "value 1" ]; check $? "a second command works"
st=$(dstat)
grep -q '^link  *udp; 1 made' <<<"$st"; check $? "on the same link, without a new connection" "$st"
[ "$(daemons)" = 1 ]; check $? "and the same daemon"
spared=$(grep '^scans' <<<"$st" | grep -oE '[0-9]+ spared' | cut -d' ' -f1)
[ "${spared:-0}" -ge 1 ]; check $? "a command in an unchanged worktree needs no scan ($spared spared)" "$st"

echo "== changes travel as they happen"
files=$(sync_n 'files received'); asked=$(sync_n 'files asked for')
sed -i 's/{ 1 }/{ 2 }/' crates/util/src/lib.rs
for i in $(seq 1 50); do
    [ "$(sync_n 'files received')" -gt "$files" ] && break
    sleep 0.1
done
[ "$(sync_n 'files received')" = $((files + 1)) ]; check $? "an edit reaches the server without any command ($files -> $(sync_n 'files received') files)" "$(R --rargo-status | grep '^sync')"
out=$(R run -q 2>&1); [ "$out" = "value 2" ]; check $? "the build that follows sees the edit" "$out"
[ "$(sync_n 'files asked for')" = "$asked" ]; check $? "and the server had nothing to ask for" "$(R --rargo-status | grep '^sync')"
# An edit and a build in the same instant: the change rides ahead of the
# request, in order with it.
sed -i 's/{ 2 }/{ 3 }/' crates/util/src/lib.rs; out=$(R run -q 2>&1)
[ "$out" = "value 3" ]; check $? "a build started in the instant of an edit sees it" "$out"
[ "$(sync_n 'files asked for')" = "$asked" ]; check $? "again without the server asking" "$(R --rargo-status | grep '^sync')"
# A new directory with a new crate in it.
mkdir -p crates/extra/src
printf '[package]\nname = "extra"\nversion = "0.1.0"\nedition = "2021"\n' >crates/extra/Cargo.toml
echo 'pub fn more() -> i32 { 40 }' >crates/extra/src/lib.rs
printf 'extra = { path = "../extra" }\n' >>crates/app/Cargo.toml
echo 'fn main() { println!("value {}", util::value() + extra::more()); }' >crates/app/src/main.rs
out=$(R run -q 2>&1); [ "$out" = "value 43" ]; check $? "a crate added in a new directory is built" "$out"
sleep 0.5
echo 'pub fn more() -> i32 { 50 }' >crates/extra/src/lib.rs
out=$(R run -q 2>&1); [ "$out" = "value 53" ]; check $? "and an edit inside the new directory is seen" "$out"
git -c user.name=t -c user.email=t@example.invalid commit -qam two
git checkout -q -b side HEAD~1 2>/dev/null
out=$(R run -q 2>&1); [ "$out" = "value 1" ]; check $? "a checkout of another commit is followed" "$out"
git checkout -q master 2>/dev/null || git checkout -q main 2>/dev/null
out=$(R run -q 2>&1); [ "$out" = "value 53" ]; check $? "and the checkout back" "$out"

echo "== the client's address changes"
ip -n ${RIG:-rargo}-cl addr del 10.77.0.200/24 dev eth0
ip -n ${RIG:-rargo}-cl addr add 10.77.0.201/24 dev eth0
sed -i 's/{ 50 }/{ 60 }/' crates/extra/src/lib.rs
out=$(R run -q 2>&1); [ "$out" = "value 63" ]; check $? "a build from the new address works" "$out"
st=$(dstat)
grep -q '^link  *udp; 1 made' <<<"$st"; check $? "on the link it had: no new connection, no new key agreement" "$st"

echo "== the daemon dies"
pkill -KILL -f "$RARGO_BIN/rargo --rargo-daemon $E"; sleep 0.3
sed -i 's/{ 60 }/{ 70 }/' crates/extra/src/lib.rs
out=$(R run -q 2>&1); [ "$out" = "value 73" ]; check $? "the next command starts a new daemon and builds" "$out"
[ "$(daemons)" = 1 ]; check $? "one daemon is running again"

echo "== the server is restarted"
"$here/netns.sh" reset-server >/dev/null 2>&1
sed -i 's/{ 70 }/{ 80 }/' crates/extra/src/lib.rs
out=$(R run -q 2>&1); [ "$out" = "value 83" ]; check $? "the daemon connects again and the build works" "$out"
st=$(dstat)
grep -q '^link  *udp; 2 made' <<<"$st"; check $? "over a second link" "$st"

echo "== a command that is interrupted"
mkdir -p crates/slow/src
printf '[package]\nname = "slow"\nversion = "0.1.0"\nedition = "2021"\nbuild = "build.rs"\n' >crates/slow/Cargo.toml
echo 'fn main() { std::thread::sleep(std::time::Duration::from_secs(60)); }' >crates/slow/build.rs
echo '' >crates/slow/src/lib.rs
nsenter --net=/run/netns/${RIG:-rargo}-cl "$RARGO_BIN/rargo" build -p slow >/dev/null 2>&1 &
cl=$!
for i in $(seq 1 100); do
    pgrep -f 'build/slow/[0-9a-f]*/out/build_script_build' >/dev/null && break
    sleep 0.1
done
pgrep -f 'build/slow/[0-9a-f]*/out/build_script_build' >/dev/null; check $? "a slow build script is running on the server"
pkill -INT -P $cl 2>/dev/null; kill -INT $cl 2>/dev/null; wait $cl 2>/dev/null
for i in $(seq 1 50); do
    pgrep -f 'build/slow/[0-9a-f]*/out/build_script_build' >/dev/null || break
    sleep 0.1
done
! pgrep -f 'build/slow/[0-9a-f]*/out/build_script_build' >/dev/null; check $? "interrupting the command stops it there, though the daemon and its link stay"
rm -rf crates/slow
out=$(R run -q 2>&1); [ "$out" = "value 83" ]; check $? "the next command works on the same link" "$out"
grep -q '^link  *udp; 2 made' <<<"$(dstat)"; check $? "(no new connection)" "$(dstat)"

echo "== without a daemon"
out=$(RARGO_NO_DAEMON=1 R run -q 2>&1); [ "$out" = "value 83" ]; check $? "RARGO_NO_DAEMON=1 runs the command in one process" "$out"

echo "== UDP does not get through"
# Drop every UDP datagram the client sends (in its own namespace).
ip netns exec ${RIG:-rargo}-cl tc qdisc replace dev eth0 root handle 1: prio
ip netns exec ${RIG:-rargo}-cl tc filter add dev eth0 parent 1: protocol ip u32 match ip protocol 17 0xff action drop
export RARGO_CACHE_DIR=$E/cache-tcp
start=$(date +%s.%N)
out=$(R run -q 2>&1); rc=$?
took=$(echo "$(date +%s.%N) - $start" | bc)
[ "$out" = "value 83" ]; check $? "a new daemon still builds (${took}s for its first command)" "$out"
st=$(dstat)
grep -q '^link  *tcp; 1 made' <<<"$st"; check $? "over TCP" "$st"
sed -i 's/{ 80 }/{ 90 }/' crates/extra/src/lib.rs
start=$(date +%s.%N)
out=$(R run -q 2>&1)
took=$(echo "$(date +%s.%N) - $start" | bc)
[ "$out" = "value 93" ]; check $? "and keeps to it for the commands that follow (${took}s after an edit)" "$out"
out=$(RARGO_TRANSPORT=udp RARGO_NO_DAEMON=1 RARGO_CACHE_DIR=$E/cache-udp R build 2>&1); rc=$?
[ $rc = 101 ] && grep -q "cannot reach the rargo server" <<<"$out"; check $? "RARGO_TRANSPORT=udp does not fall back" "$out"
ip netns exec ${RIG:-rargo}-cl tc qdisc del dev eth0 root 2>/dev/null

echo "== the output directory is virtual"
export RARGO_CACHE_DIR=$E/cache
mounted() { grep -q " $proj/target [^ ]* [^ ]* - fuse.rargo " /proc/self/mountinfo; }
shown() { dstat | grep '^outputs' | grep -oE "[0-9]+ $1" | head -1 | cut -d' ' -f1; }
out=$(R build 2>&1); check $? "a build through the first daemon again" "$out"
mounted; check $? "target/ is a file system of the daemon's" "$(grep "$proj" /proc/self/mountinfo)"
[ "$(shown 'worktrees with')" = 1 ]; check $? "which shows what the server built" "$(dstat)"
before=$(shown 'bytes fetched')
sed -i 's/{ 90 }/{ 100 }/' crates/extra/src/lib.rs
out=$(R build 2>&1); check $? "a build after an edit" "$out"
[ -x target/debug/app ] && [ "$(stat -c %s target/debug/app)" -gt 100000 ]; check $? "the new binary is listed with its size at once"
[ "$(shown 'bytes fetched')" = "$before" ]; check $? "without its content having crossed the link ($before bytes fetched before and after)" "$(dstat)"
out=$(./target/debug/app 2>&1); [ "$out" = "value 103" ]; check $? "running it fetches it" "$out"
after=$(shown 'bytes fetched')
[ "$after" -gt "$before" ]; check $? "($((after - before)) bytes fetched, $(shown 'bytes reused') reused from the cache)" "$(dstat)"
real=$(find "$E/cache/blobs" -type f | wc -l)
! find target -maxdepth 0 -newer "$E/cache" -type f | grep -q .; [ "$(shown 'made real')" = 0 ]; check $? "nothing was copied into the worktree ($(shown 'made real') files made real)" "$(dstat)"
# Stock cargo in the same directory: it reads what is shown and writes its own.
out=$(cargo build --offline 2>&1); check $? "stock cargo builds in the same target/" "$out"
out=$(./target/debug/app 2>&1); [ "$out" = "value 103" ]; check $? "and its binary runs" "$out"
out=$(cargo test --offline -q 2>&1); check $? "stock cargo test works there" "$out"
sed -i 's/{ 100 }/{ 110 }/' crates/extra/src/lib.rs
out=$(R run -q 2>&1); [ "$out" = "value 113" ]; check $? "rargo after stock cargo: the new build replaces what stock cargo left" "$out"
out=$(./target/debug/app 2>&1); [ "$out" = "value 113" ]; check $? "(the file in target/ is the new one)" "$out"
# A directory is listed in several reads while compilers add and remove
# their temporaries: what stays must always be listed.
mkdir -p target/stress
for i in $(seq 1 400); do : >"target/stress/keep-with-a-long-enough-name-$i"; done
( for r in $(seq 1 150); do for j in 1 2 3 4 5 6 7 8 9 10 11 12; do : >"target/stress/a-temporary-$j"; done; rm -f target/stress/a-temporary-*; done ) &
churn=$!
short=0
for r in $(seq 1 150); do [ "$(ls target/stress | grep -c '^keep')" = 400 ] || short=$((short + 1)); done
wait $churn
[ $short = 0 ]; check $? "a listing read while files come and go names every file that stays ($short of 150 listings were short)"
rm -rf target/stress
# Copying as a fallback, on request.
out=$(RARGO_OUTPUT=copy R build 2>&1); check $? "RARGO_OUTPUT=copy builds" "$out"
! mounted; check $? "and target/ is a plain directory again"
out=$(./target/debug/app 2>&1); [ "$out" = "value 113" ]; check $? "with the binary copied into it" "$out"
out=$(R build 2>&1); mounted; check $? "the next command without it shows results again" "$out"
out=$(./target/debug/app 2>&1); [ "$out" = "value 113" ]; check $? "(same binary)" "$out"
# cargo clean has to be able to remove the directory.
out=$(R clean 2>&1); rc=$?
[ $rc = 0 ] && [ ! -e target ]; check $? "clean removes target/" "$out"
out=$(R run -q 2>&1); [ "$out" = "value 113" ]; check $? "and the next build fills it again" "$out"
# A daemon that is killed outright cannot unmount. What it left watching
# does, at once and with no rargo command run: target/ is never a dead
# mount, and stock cargo works in the worktree.
unmounted() { for _ in $(seq 1 50); do grep -q " $1 [^ ]* [^ ]* - fuse.rargo " /proc/self/mountinfo || return 0; sleep 0.1; done; return 1; }
# (The daemon of the TCP part above is still running, with a cache of its
# own; it is not this one, and its mount here was replaced long ago.)
mine() { pgrep -f "$RARGO_BIN/rargo --rargo-daemon $E/cache/"; }
guard=$(pgrep -P "$(mine)" -f "^unmount-guard: $proj/target")
[ -n "$guard" ]; check $? "a process of the daemon's watches the mount, and not under the daemon's name" "$(pgrep -af unmount-guard)"
pkill -KILL -f "$RARGO_BIN/rargo --rargo-daemon $E/cache/"
unmounted "$proj/target"; check $? "after the daemon is killed (SIGKILL) target/ is unmounted, with no rargo command run" "$(grep "$proj" /proc/self/mountinfo)"
out=$(ls target 2>&1); check $? "and is a plain directory that can be listed" "$out"
[ -z "$(mine)" ]; check $? "(its daemon is not running)" "$(mine)"
out=$(cargo build --offline 2>&1); check $? "stock cargo builds in the worktree after the daemon was killed" "$out"
out=$(./target/debug/app 2>&1); [ "$out" = "value 113" ]; check $? "and its binary runs" "$out"
out=$(cargo test --offline -q 2>&1); check $? "stock cargo test works there too" "$out"
! mounted && [ -z "$(mine)" ]; check $? "all of it without rargo: still no daemon and no mount"
! kill -0 "$guard" 2>/dev/null; check $? "and what watched the mount is gone"
out=$(R run -q 2>&1); [ "$out" = "value 113" ]; check $? "the next rargo command starts a daemon and builds" "$out"
mounted; check $? "which mounts target/ again"
out=$(./target/debug/app 2>&1); [ "$out" = "value 113" ]; check $? "and shows the binary" "$out"

echo "== a user who may not mount"
# An unprivileged user gets the virtual directory through fusermount3.
# The user has to be able to reach the files, which nothing under /root
# allows: this one part of the suite uses a directory in /tmp.
U=$(mktemp -d /tmp/rargo-e2e-user.XXXXXX)
mkdir -p "$U/bin" "$U/home"
cp "$RARGO_BIN/rargo" "$U/bin/"
cp -r "$proj/Cargo.toml" "$proj/crates" "$U/"
[ -f "$proj/Cargo.lock" ] && cp "$proj/Cargo.lock" "$U/"
chown -R nobody:nogroup "$U"; chmod 755 "$U"
asuser() { (cd "$U" && nsenter --net=/run/netns/${RIG:-rargo}-cl setpriv --reuid=nobody --regid=nogroup --clear-groups \
    env HOME="$U/home" RARGO_CACHE_DIR="$U/cache" "$@"); }
out=$(asuser "$U/bin/rargo" build 2>&1); check $? "an unprivileged user builds" "$out"
grep -q " $U/target [^ ]* [^ ]* - fuse.rargo " /proc/self/mountinfo; check $? "and has the virtual output directory (mounted through fusermount3)" "$(grep "$U" /proc/self/mountinfo; tail -3 "$U/cache/daemon.log")"
out=$(asuser "$U/target/debug/app" 2>&1); [ "$out" = "value 113" ]; check $? "the binary runs out of it" "$out"
out=$(asuser sh -c 'echo x >target/note && cat target/note' 2>&1); [ "$out" = x ]; check $? "the user can write there" "$out"
# This user's daemon killed: the mount was made by fusermount3, and is
# unmounted through it.
pkill -KILL -f "$U/bin/rargo --rargo-daemon "
unmounted "$U/target"; check $? "a killed daemon of this user leaves no mount either" "$(grep "$U" /proc/self/mountinfo)"
out=$(asuser cat target/note 2>&1); [ "$out" = x ]; check $? "and what the user wrote is in the plain directory" "$out"
out=$(asuser "$U/bin/rargo" build 2>&1); check $? "the user's next build starts a daemon again" "$out"
grep -q " $U/target [^ ]* [^ ]* - fuse.rargo " /proc/self/mountinfo; check $? "which mounts target/ again" "$(tail -3 "$U/cache/daemon.log")"
asuser "$U/bin/rargo" --rargo-daemon-stop; sleep 0.5
! grep -q " $U/target " /proc/self/mountinfo; check $? "stopping the daemon unmounts it"
[ -f "$U/target/note" ]; check $? "and what the user wrote is in the plain directory"
rm -rf "$U"

echo "== daemons go when told"
RARGO_CACHE_DIR=$E/cache-tcp R --rargo-daemon-stop; RARGO_CACHE_DIR=$E/cache R --rargo-daemon-stop
sleep 0.5
[ "$(daemons)" = 0 ]; check $? "--rargo-daemon-stop ends them ($(daemons) left)"
! mounted && [ -d target ]; check $? "and leaves target/ a plain directory"
out=$(cargo build --offline 2>&1); check $? "in which stock cargo builds" "$out"
out=$(./target/debug/app 2>&1); [ "$out" = "value 113" ]; check $? "the same program" "$out"
grep -c "panicked" "$work/${RIG_STATE:-nstest}/srv.log" "$E"/cache*/daemon.log 2>/dev/null | grep -v ':0$' | { ! grep -q .; }; check $? "nothing panicked"

cd /
"$here/netns.sh" down >/dev/null 2>&1
umount -l "$work/${RIG_STATE:-nstest}/srv/mnt" 2>/dev/null
rm -rf "$E"
echo
echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
