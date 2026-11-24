#!/bin/bash
# The gate before a release: every suite, on this machine, on the working
# tree's release build. Nobody reviews merges; this is what stands in.
#
#   dev/gate.sh            prints one line per suite; exits non-zero if any failed
#
# The rig suites run in a second rig (RIG=rargo2) so that a benchmark
# holding the first is not disturbed. Logs: $RARGO_WORK/gate/.
set -u
work=${RARGO_WORK:-/root/rargo-work}
here=$(cd "$(dirname "$0")" && pwd)
. "$work/env.sh"
cd "$here/.." || exit 2
L=$work/gate; mkdir -p "$L"
jobs=${RARGO_JOBS:-4}
cap() { systemd-run --scope -q -p CPUQuota=600% -p MemoryMax=30G "$@"; }
fail=0
step() { # name, command...
    local name=$1; shift
    if "$@" >"$L/$name.log" 2>&1; then echo "ok    $name: $(grep -E 'passed|test result' "$L/$name.log" | tail -n 1 | sed 's/^ *//')"
    else echo "FAIL  $name (see $L/$name.log)"; fail=1; fi
}
step build cap cargo build --release --locked
step unit cap cargo test --release --locked --workspace
B=$work/gate/bin; rm -rf "$B"; mkdir -p "$B"; cp target/release/{rargo,rargod,rargo-sandbox} "$B/"
export RARGO_BIN=$B RARGO_JOBS=$jobs
step e2e dev/e2e.sh
step e2e-copy env RARGO_OUTPUT=copy dev/e2e.sh
step e2e-flake dev/e2e-flake.sh
step e2e-link env RIG=rargo2 RIG_STATE=rig2 dev/e2e-link.sh
step e2e-worker env RIG=rargo2 RIG_STATE=rig2 dev/e2e-worker.sh
git log --oneline -1
[ -z "$(git status --porcelain)" ] || echo "note: the working tree has uncommitted changes"
exit $fail
