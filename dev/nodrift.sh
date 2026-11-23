#!/bin/bash
# No-drift check for a benchmark repository: after rargo has built and tested
# it, the repository must still build and test with stock cargo, in the same
# worktree and the same target directory rargo linked artifacts into.
#   dev/nodrift.sh REPO 'test args for stock cargo' ['rargo test args']
# Needs a running rargod (RARGO_SOCKET) and RARGO_BIN.
set -u
work=${RARGO_WORK:-/root/rargo-work}
. "$work/env.sh"
repo=$1; tests=$2; rtests=${3:-$2}
R=${RARGO_BIN:?}/rargo
cd "$work/bench/$repo" || exit 2
fails=0
say() { printf '%-64s %s\n' "$1" "$2"; }
step() { # description, command...
    local d=$1; shift
    local t0=$(date +%s.%N)
    if out=$("$@" 2>&1); then say "$d" "ok ($(printf '%.0f' "$(echo "$(date +%s.%N) - $t0" | bc)") s)"
    else say "$d" "FAILED"; echo "$out" | tail -25 | sed 's/^/    | /'; fails=$((fails + 1)); fi
}
cap() { systemd-run --scope -q -p CPUQuota=2400% -p MemoryMax=90G "$@"; }
before=$(git status --porcelain | grep -v '^??' | sort)
step "rargo build" "$R" build
step "rargo test $rtests" "$R" test $rtests
step "rargo: drift check (stock cargo plan vs rargo)" "$R" --rargo-drift-check
after=$(git status --porcelain | grep -v '^??' | sort)
[ "$before" = "$after" ] && say "tracked files untouched by rargo" ok || { say "tracked files untouched by rargo" FAILED; fails=$((fails + 1)); }
untracked=$(git status --porcelain --ignored | grep -v ' target/$' | grep -v '^!! target' || true)
[ -z "$untracked" ] && say "nothing left in the worktree outside target/" ok || { say "nothing left in the worktree outside target/" "note:"; echo "$untracked" | head -5 | sed 's/^/    | /'; }
step "stock cargo build, same worktree and target/" cap cargo build --offline -j24
step "stock cargo test $tests" cap cargo test --offline -j24 $tests
step "rargo build again after stock cargo used target/" "$R" build
echo "$repo: $fails failure(s)"
exit $fails
