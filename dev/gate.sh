#!/bin/bash
# Run the suites on the checked-out commit and publish their results.
set -euo pipefail
work=${RARGO_WORK:-/root/rargo-work}
here=$(cd "$(dirname "$0")" && pwd)
cd "$here/.."
repo=sadosystems/rargo2
clean() {
    [ -z "$(git status --porcelain)" ]
}
clean || { echo "gate requires a clean working tree" >&2; exit 2; }
commit=$(git rev-parse HEAD)
L=${RARGO_GATE_DIR:-$work/gate}
mkdir -p "$L"
. "$work/env.sh"
jobs=${RARGO_JOBS:-4}
cap() { systemd-run --scope -q -p CPUQuota=600% -p MemoryMax=30G "$@"; }
report() {
    gh api --method POST "repos/$repo/statuses/$commit" \
        -f "context=gate/$1" -f "state=$2" -f "description=${3:0:140}" >/dev/null
}
summary() {
    awk '
        /test result:/ { pass += $4; fail += $6; ignored += $8; unit = 1 }
        /^e2e.*: (all|[0-9]+ of)/ || /^[0-9]+ passed, [0-9]+ failed$/ { suite = $0 }
        END {
            if (unit) printf "%d passed, %d failed, %d ignored", pass, fail, ignored
            else if (suite) print suite
        }
    ' "$1"
}
suites=(build unit e2e e2e-copy e2e-flake e2e-auth e2e-link e2e-worker)
report all pending "Running ${#suites[@]} gate steps"
for suite in "${suites[@]}"; do report "$suite" pending "Waiting for gate"; done
passed=0
step() {
    local name=$1 status description log=$L/$1.log
    shift
    if "$@" >"$log" 2>&1; then
        status=success
        passed=$((passed + 1))
        description=$(summary "$log")
        echo "ok    $name: ${description:-passed}"
    else
        status=failure
        description=$(summary "$log")
        echo "FAIL  $name (see $log)"
    fi
    report "$name" "$status" "${description:-$status}"
}
step build cap cargo build --release --locked
step unit cap cargo test --release --locked --workspace
if [ "$passed" -ne 2 ]; then
    for suite in "${suites[@]:2}"; do report "$suite" error "Build or unit tests failed"; done
    report all failure "$passed of ${#suites[@]} gate steps passed"
    exit 1
fi
B=$L/bin
mkdir -p "$B"
cp "${CARGO_TARGET_DIR:-target}"/release/{rargo,rargod,rargo-sandbox} "$B/"
export RARGO_BIN=$B RARGO_JOBS=$jobs
export RIG=${RIG:-rargo2} RIG_STATE=${RIG_STATE:-rig2}
step e2e dev/e2e.sh
step e2e-copy env RARGO_OUTPUT=copy dev/e2e.sh
step e2e-flake dev/e2e-flake.sh
step e2e-auth dev/e2e-auth.sh
step e2e-link dev/e2e-link.sh
step e2e-worker dev/e2e-worker.sh
if ! clean || [ "$(git rev-parse HEAD)" != "$commit" ]; then
    report all error "Checkout changed during the gate"
    exit 2
fi
result="$passed of ${#suites[@]} gate steps passed"
if [ "$passed" -eq "${#suites[@]}" ]; then
    report all success "$result"
else
    report all failure "$result"
    exit 1
fi
