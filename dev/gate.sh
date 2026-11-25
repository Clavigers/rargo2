#!/bin/bash
# Run the suites on the checked-out commit and publish their results.
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
# Every suite's files are under this gate's directory (the mount tests'
# scratch area too), so that two gates with different gate directories
# share nothing.
mkdir -p "$L/work/tmp"
export RARGO_TEST_TMP=${RARGO_TEST_TMP:-$L/work/tmp}
export RARGO_E2E_ROOT=${RARGO_E2E_ROOT:-$L/work/tmp}
# Use the repository's build server.
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
# The copy replaces these files, which it can do only when nothing still
# has them mapped. A suite's daemon exits on SIGTERM. The process that
# unmounts its mount ignores that signal and exits once the daemon is gone.
release_binaries() {
    local sig skip pid exe comm i
    for sig in TERM KILL; do
        for pid in /proc/[0-9]*; do
            [ -d "$pid" ] || continue
            pid=${pid#/proc/}
            exe=$(readlink "/proc/$pid/exe" 2>/dev/null || true)
            case "$exe" in
                "$B/rargo"|"$B/rargo (deleted)"|"$B/rargod"|"$B/rargod (deleted)"|"$B/rargo-sandbox"|"$B/rargo-sandbox (deleted)") ;;
                *) continue ;;
            esac
            comm=$(cat "/proc/$pid/comm" 2>/dev/null || true)
            # Leave the guard until its daemon is gone, so the mount goes too.
            [ "$sig" = TERM ] && [ "$comm" = unmount-guard ] && continue
            kill "-$sig" "$pid" 2>/dev/null || true
        done
        for i in $(seq 1 30); do
            skip=0
            for pid in /proc/[0-9]*; do
                [ -d "$pid" ] || continue
                pid=${pid#/proc/}
                exe=$(readlink "/proc/$pid/exe" 2>/dev/null || true)
                case "$exe" in
                    "$B/rargo"|"$B/rargo (deleted)"|"$B/rargod"|"$B/rargod (deleted)"|"$B/rargo-sandbox"|"$B/rargo-sandbox (deleted)") skip=1; break ;;
                esac
            done
            [ "$skip" = 0 ] && return 0
            sleep 0.1
        done
    done
    echo "gate: a process is still running a binary in $B" >&2
    return 1
}
release_binaries
cp target/release/{rargo,rargod,rargo-sandbox} "$B/"
export RARGO_BIN=$B RARGO_JOBS=$jobs
# The suites that follow run at the same time. Each rargod they start
# through dev/rargod.sh stays inside a share of the machine, so the
# service beside them keeps the headroom CLAUDE.md asks for. A rig's own
# caps are set in dev/netns.sh from the cores that rig is given.
export RARGO_CPU_QUOTA=${RARGO_CPU_QUOTA:-800%}
export RARGO_MEMORY_MAX=${RARGO_MEMORY_MAX:-20G}
# One rig name for this gate. Suites that use a rig append their own
# suffix, so each has namespaces and state of its own and a benchmark's
# rig is left alone.
export RIG=${RIG:-g$(printf '%s' "$L" | sha256sum | cut -c1-6)} RIG_STATE=${RIG_STATE:-$L/work/rig}
# Each suite's directory is under this gate's directory, and the suites
# that listen on fixed ports get free ones. What the environment already
# names stays.
free_pair() { # a port P such that P and P + 1 are free on loopback
    python3 -I -c '
import socket
for _ in range(200):
    a = socket.socket(); a.bind(("127.0.0.1", 0)); p = a.getsockname()[1]
    b = socket.socket()
    try:
        b.bind(("127.0.0.1", p + 1))
    except OSError:
        continue
    print(p)
    break
'
}
export RARGO_E2E_FLAKE=${RARGO_E2E_FLAKE:-$L/work/e2e-flake}
export RARGO_E2E_FLAKE_PORT=${RARGO_E2E_FLAKE_PORT:-$(free_pair)}
export RARGO_E2E_AUTH=${RARGO_E2E_AUTH:-$L/work/e2e-auth}
export E2E_AUTH_PORT=${E2E_AUTH_PORT:-$(free_pair)}
export RARGO_E2E_LINK=${RARGO_E2E_LINK:-$L/work/e2e-link}
export RARGO_E2E_WORKER=${RARGO_E2E_WORKER:-$L/work/e2e-worker}
# The toolchains e2e-components installs components into: kept between
# runs of this gate.
export RARGO_COMPONENTS_RUSTUP=${RARGO_COMPONENTS_RUSTUP:-$L/work/rustup-components}
# mktemp needs the parent of each suite's directory to exist already.
mkdir -p "$L/work/e2e" "$L/work/e2e-copy" "$L/work/e2e-components"
# Directories, rigs and ports are already per suite. They run together;
# the gate's time is the slowest of them, plus the build above.
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
if [ "$passed" -eq "${#suites[@]}" ]; then
    report all success "$result"
else
    report all failure "$result"
    exit 1
fi
