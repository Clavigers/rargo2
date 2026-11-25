#!/bin/bash
set -euo pipefail
gate_started=$(date +%s)
work=${RARGO_WORK:-/root/rargo-work}
here=$(cd "$(dirname "$0")" && pwd)
cd "$here/.."
repo=sadosystems/rargo2
clean() {
    [ -z "$(git status --porcelain)" ]
}
clean || { echo "gate requires a clean working tree" >&2; exit 2; }
dev/gate-budget.sh raise-ok || exit 2
commit=$(git rev-parse HEAD)
L=${RARGO_GATE_DIR:-$work/gate}
mkdir -p "$L"
. "$work/env.sh"
jobs=${RARGO_JOBS:-4}
report() {
    gh api --method POST "repos/$repo/statuses/$commit" \
        -f "context=gate/$1" -f "state=$2" -f "description=${3:0:140}" >/dev/null
}
summary() {
    awk '
        $1 == "test" && $2 == "result:" { pass += $4; fail += $6; ignored += $8; unit = 1 }
        /^e2e.*: (all|[0-9]+ of)/ || /^[0-9]+ passed, [0-9]+ failed$/ { suite = $0 }
        END {
            if (unit) printf "%d passed, %d failed, %d ignored", pass, fail, ignored
            else if (suite) print suite
        }
    ' "$1"
}
suites=(build unit mount-tests e2e e2e-copy e2e-flake e2e-auth e2e-components e2e-link e2e-worker)
report all pending "Running ${#suites[@]} gate steps"
for suite in "${suites[@]}"; do report "$suite" pending "Waiting for gate"; done
passed=0
step() {
    local name=$1 status description log=$L/$1.log started
    shift
    started=$(date +%s)
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
    echo $(( $(date +%s) - started )) >"$L/$name.secs"
    report "$name" "$status" "${description:-$status}"
}
mkdir -p "$L/work/tmp"
export RARGO_TEST_TMP=${RARGO_TEST_TMP:-$L/work/tmp}
export RARGO_E2E_ROOT=${RARGO_E2E_ROOT:-$L/work/tmp}
unset CARGO_TARGET_DIR
step build rargo build --release --locked
step unit rargo test --release --locked --workspace --lib --bins
step mount-tests dev/mount-tests.sh --release
if [ "$passed" -ne 3 ]; then
    for suite in "${suites[@]:3}"; do report "$suite" error "Build or tests failed"; done
    report all failure "$passed of ${#suites[@]} gate steps passed"
    exit 1
fi
B=$L/bin
mkdir -p "$B"
release_binaries() {
    local pid comm
    bin_pids() {
        local p e out=""
        for p in /proc/[0-9]*; do
            [ -d "$p" ] || continue
            p=${p#/proc/}
            e=$(readlink "/proc/$p/exe" 2>/dev/null || true)
            case "$e" in
                "$B/rargo"|"$B/rargo (deleted)"|"$B/rargod"|"$B/rargod (deleted)"|"$B/rargo-sandbox"|"$B/rargo-sandbox (deleted)")
                    out="$out $p" ;;
            esac
        done
        printf '%s\n' "$out"
    }
    for pid in $(bin_pids); do
        comm=$(cat "/proc/$pid/comm" 2>/dev/null || true)
        [ "$comm" = unmount-guard ] && continue
        kill -TERM "$pid" 2>/dev/null || true
    done
    for pid in $(bin_pids); do
        comm=$(cat "/proc/$pid/comm" 2>/dev/null || true)
        [ "$comm" = unmount-guard ] && continue
        tail --pid="$pid" -f /dev/null
    done
    for pid in $(bin_pids); do
        kill -KILL "$pid" 2>/dev/null || true
    done
    for pid in $(bin_pids); do
        tail --pid="$pid" -f /dev/null
    done
    if [ -n "$(bin_pids | tr -d '[:space:]')" ]; then
        echo "gate: a process is still running a binary in $B" >&2
        return 1
    fi
}
release_binaries
cp target/release/{rargo,rargod,rargo-sandbox} "$B/"
export RARGO_BIN=$B RARGO_JOBS=$jobs
export RARGO_CPU_QUOTA=${RARGO_CPU_QUOTA:-800%}
export RARGO_MEMORY_MAX=${RARGO_MEMORY_MAX:-20G}
export RIG=${RIG:-g$(printf '%s' "$L" | sha256sum | cut -c1-6)} RIG_STATE=${RIG_STATE:-$L/work/rig}
free_ports() {
    local n
    n=$(sed -n 's/^ports=\([0-9]\+\)$/\1/p' "dev/$1.sh")
    [ -n "$n" ] || { echo "dev/$1.sh has no ports=N line" >&2; exit 2; }
    python3 -I dev/free-ports.py "$n"
}
export RARGO_E2E_FLAKE=${RARGO_E2E_FLAKE:-$L/work/e2e-flake}
export RARGO_E2E_FLAKE_PORT=${RARGO_E2E_FLAKE_PORT:-$(free_ports e2e-flake)}
export RARGO_E2E_AUTH=${RARGO_E2E_AUTH:-$L/work/e2e-auth}
export E2E_AUTH_PORT=${E2E_AUTH_PORT:-$(free_ports e2e-auth)}
export RARGO_E2E_LINK=${RARGO_E2E_LINK:-$L/work/e2e-link}
export RARGO_E2E_WORKER=${RARGO_E2E_WORKER:-$L/work/e2e-worker}
export RARGO_COMPONENTS_RUSTUP=${RARGO_COMPONENTS_RUSTUP:-$L/work/rustup-components}
mkdir -p "$L/work/e2e" "$L/work/e2e-copy" "$L/work/e2e-components"
declare -A suite_pid
launch() {
    local name=$1
    shift
    (
        set +e
        started=$(date +%s)
        "$@" >"$L/$name.log" 2>&1
        echo $? >"$L/$name.rc"
        echo $(( $(date +%s) - started )) >"$L/$name.secs"
    ) &
    suite_pid[$name]=$!
}
launch e2e env RARGO_E2E_ROOT="$L/work/e2e" dev/e2e.sh
launch e2e-copy env RARGO_OUTPUT=copy RARGO_E2E_ROOT="$L/work/e2e-copy" dev/e2e.sh
launch e2e-flake dev/e2e-flake.sh
launch e2e-auth dev/e2e-auth.sh
launch e2e-components env RARGO_E2E_ROOT="$L/work/e2e-components" dev/e2e-components.sh
launch e2e-link env RIG="${RIG}l" RIG_STATE="$L/work/rig-link" dev/e2e-link.sh
launch e2e-worker env RIG="${RIG}w" RIG_STATE="$L/work/rig-worker" dev/e2e-worker.sh
for suite in e2e e2e-copy e2e-flake e2e-auth e2e-components e2e-link e2e-worker; do
    wait "${suite_pid[$suite]}" || true
    rc=$(cat "$L/$suite.rc" 2>/dev/null || echo 1)
    took=$(cat "$L/$suite.secs" 2>/dev/null || echo "?")
    if [ "$rc" = 0 ]; then
        passed=$((passed + 1))
        description=$(summary "$L/$suite.log")
        echo "ok    $suite (${took}s): ${description:-passed}"
        report "$suite" success "${description:-passed}"
    else
        description=$(summary "$L/$suite.log")
        echo "FAIL  $suite (${took}s, see $L/$suite.log)"
        report "$suite" failure "${description:-failure}"
    fi
done
if ! clean || [ "$(git rev-parse HEAD)" != "$commit" ]; then
    report all error "Checkout changed during the gate"
    exit 2
fi
result="$passed of ${#suites[@]} gate steps passed"
elapsed=$(( $(date +%s) - gate_started ))
echo "gate ${elapsed}s: $result"
if [ "$passed" -eq "${#suites[@]}" ] && ! dev/gate-budget.sh check "$elapsed" "$L" "${suites[@]}"; then
    report all failure "gate ${elapsed}s is over the budget in dev/gate-budget"
    exit 1
fi
if [ "$passed" -eq "${#suites[@]}" ]; then
    dev/gate-budget.sh record "$L" "${suites[@]}"
    report all success "$result"
else
    report all failure "$result"
    exit 1
fi
