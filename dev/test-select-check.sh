#!/bin/bash
set -u
here=$(cd "$(dirname "$0")" && pwd)
work=${RARGO_WORK:-/root/rargo-work}
: "${RARGO_BIN:?RARGO_BIN names the directory of the rargo and rargod to check}"
export BENCH_STATE=${BENCH_STATE:-test-select} BENCH_PORT=${BENCH_PORT:-7611}
. "$here/bench-lib.sh"
out=${TSC_OUT:-$work/test-select}
R=$RARGO_BIN/rargo

server_up() {
    bench_up "${TSC_JOBS:-16}" 0 0 0 >/dev/null || exit 2
    . "$T/client.env"
    export RARGO_CACHE_DIR=$T/client-cache
}

server_down() {
    bench_down
}

compare_commit() {
    local tree=$1 commit=$2; shift 2
    git -C "$tree" checkout -q --detach "$commit" || return 2
    local short=$(git -C "$tree" rev-parse --short=10 HEAD)
    (cd "$tree" && "$R" test --no-fail-fast "$@") >"$out/$short.cached.log" 2>&1
    (cd "$tree" && "$R" test --no-fail-fast --no-cache "$@") >"$out/$short.real.log" 2>&1
    python3 -I "$here/test-select-compare.py" "$short" "$out/$short.cached.log" "$out/$short.real.log" | tee -a "$out/summary"
}

totals() {
    awk '/ compared=/ { n++; split($2, c, "="); split($3, d, "="); t += c[2]; x += d[2] } END { printf "%d commit(s), %d test outcome(s) compared, %d difference(s)\n", n, t, x }' "$out/summary"
}

drop_tree() {
    grep -q " $1/target " /proc/self/mountinfo && umount "$1/target"
    git -C "$2" worktree remove --force "$1" 2>/dev/null || rm -rf "$1"
}

replay() {
    local repo=$1 tip=$2 n=$3; shift 3
    local tree=$work/$BENCH_STATE-tree
    rm -rf "$out"; mkdir -p "$out"
    drop_tree "$tree" "$repo"
    git -C "$repo" worktree add -q --detach "$tree" "$tip" || exit 2
    server_up
    for c in $(git -C "$repo" rev-list --first-parent -n "$n" "$tip" | tac); do
        compare_commit "$tree" "$c" "$@"
    done
    server_down
    drop_tree "$tree" "$repo"
    totals
    ! grep -q '^  DIFF ' "$out/summary"
}

fixture() {
    local dir=$1
    rm -rf "$dir"; mkdir -p "$dir/src" "$dir/tests"
    git -C "$dir" init -q
    printf '[package]\nname = "fx"\nversion = "0.1.0"\nedition = "2021"\n' >"$dir/Cargo.toml"
    printf '/target\n' >"$dir/.gitignore"
    printf 'pub fn add(a: u32, b: u32) -> u32 { a + b }\n#[test] fn adds() { assert_eq!(add(1, 2), 3); }\n' >"$dir/src/lib.rs"
    printf '#[test] fn other() { assert_eq!(fx::add(2, 2), 4); }\n#[test] #[ignore] fn slow() {}\n' >"$dir/tests/other.rs"
}

commit_all() {
    git -C "$1" add -A && git -C "$1" -c user.name=test-select -c user.email=test-select@example.invalid commit -qm "$2"
}

self_check() {
    local base=$work/test-select-fixtures fails=0
    rm -rf "$out"; mkdir -p "$out"
    server_up
    fixture "$base/plain"
    commit_all "$base/plain" one
    compare_commit "$base/plain" HEAD
    printf '#[test] fn other() { assert_eq!(fx::add(2, 3), 5); }\n#[test] #[ignore] fn slow() {}\n' >"$base/plain/tests/other.rs"
    commit_all "$base/plain" two
    compare_commit "$base/plain" HEAD
    if grep -q '^  DIFF ' "$out/summary"; then echo "FAIL a plain repository shows a difference"; fails=$((fails + 1)); else echo "ok   a plain repository shows no difference"; fi
    : >"$out/summary"
    local deadline=$(( $(date +%s) + 45 ))
    fixture "$base/flaky"
    printf '#[test] fn before_the_deadline() { let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs(); assert!(now < %s); }\n' "$deadline" >"$base/flaky/tests/flaky.rs"
    commit_all "$base/flaky" one
    compare_commit "$base/flaky" HEAD
    timeout 120 bash -c "until [ \$(date +%s) -gt $deadline ]; do sleep 2; done"
    printf '#[test] fn other() { assert_eq!(fx::add(2, 3), 5); }\n#[test] #[ignore] fn slow() {}\n' >"$base/flaky/tests/other.rs"
    commit_all "$base/flaky" two
    compare_commit "$base/flaky" HEAD
    if grep -q '^  DIFF .*before_the_deadline: cached ok, real FAILED' "$out/summary"; then echo "ok   a test made flaky on purpose shows a difference"; else echo "FAIL a test made flaky on purpose shows no difference"; fails=$((fails + 1)); fi
    server_down
    for d in plain flaky; do grep -q " $base/$d/target " /proc/self/mountinfo && umount "$base/$d/target"; done
    rm -rf "$base"
    [ "$fails" -eq 0 ]
}

case "${1:-}" in
    --self-check) self_check ;;
    ""|-h|--help) echo "usage: $0 REPO TIP N [TEST ARGS...] | $0 --self-check" >&2; exit 2 ;;
    *) replay "$@" ;;
esac
