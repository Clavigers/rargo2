#!/bin/bash
# Run with a matching rargo/rargod pair, a rargo planner, and a host that can run
# rargod (including FUSE over io_uring and the bundled Nix development shell).
# Usage: dev/first-builds-test.sh BIN_DIR PLANNER
set -euo pipefail

bin=$(realpath "${1:?pass the directory containing rargo and rargod}")
planner=$(realpath "${2:?pass the path to the planner}")
export TEST_REAL_NIX
TEST_REAL_NIX=$(realpath "$(command -v nix)")
tmp=$(mktemp -d)
server=
round=
export RARGO_OUTPUT=copy
unset CARGO_TARGET_DIR RARGO_DAEMON_CONFIG

client() (
    project=$1
    shift
    cd "$tmp/$project"
    export RARGO_CACHE_DIR="$tmp/cache-$round-$project"
    export XDG_CONFIG_HOME="$tmp/config-$round-$project"
    timeout -k 5s "${BUILD_TIMEOUT:-240s}" "$bin/rargo" "$@"
)

stop_server() {
    if [[ -n "$server" ]]; then
        kill -TERM "$server" 2>/dev/null || true
        wait "$server" || true
        server=
    fi
}

cleanup() {
    local status=$?
    if [[ -n "$round" ]]; then
        ( BUILD_TIMEOUT=10s client app --rargo-daemon-stop ) >/dev/null 2>&1 &
        local app_stop=$!
        ( BUILD_TIMEOUT=10s client slow --rargo-daemon-stop ) >/dev/null 2>&1 &
        wait "$app_stop" "$!" || true
    fi
    stop_server
    if (( status != 0 )); then
        for log in "$tmp"/*.log; do
            if [[ -f "$log" ]]; then
                printf '\n--- %s ---\n' "${log##*/}" >&2
                cat "$log" >&2
            fi
        done
    fi
    rm -rf "$tmp"
}
trap cleanup EXIT

mkdir -p "$tmp/bin"
cat >"$tmp/bin/nix" <<'SH'
#!/bin/sh
# Keep the real version probe in flight while the other client connects.
for arg do
    if [ "$arg" = --version ]; then
        sleep 2
        break
    fi
done
exec "$TEST_REAL_NIX" "$@"
SH
chmod +x "$tmp/bin/nix"

for project in app slow; do
    mkdir -p "$tmp/$project/src"
    printf '[package]\nname = "%s"\nversion = "0.1.0"\nedition = "2021"\n' "$project" >"$tmp/$project/Cargo.toml"
    printf 'fn main() { println!("%s"); }\n' "$project" >"$tmp/$project/src/main.rs"
done

export RARGO_SERVER RARGO_TOKEN
RARGO_TOKEN=$("$bin/rargod" token create --state "$tmp/state" --user first-builds)
for round in fresh warm; do
    log="$tmp/server-$round.log"
    PATH="$tmp/bin:$PATH" timeout -k 5s 15m "$bin/rargod" --state "$tmp/state" \
        --planner "$planner" --listen 127.0.0.1:0 --jobs 4 --drift-interval-min 0 >"$log" 2>&1 &
    server=$!
    RARGO_SERVER=$(timeout 30s bash -c '
        awk "/^rargod: serving clients on / {print \$5; found=1; exit} END {exit !found}" \
            < <(tail -n +1 -f --pid="$1" "$2")
    ' bash "$server" "$log")

    client app build >"$tmp/app-$round.log" 2>&1 &
    app=$!
    client slow build >"$tmp/slow-$round.log" 2>&1 &
    slow=$!
    failed=0
    wait "$app" || failed=1
    wait "$slow" || failed=1
    if (( failed )); then
        echo "parallel first builds failed with a $round store" >&2
        exit 1
    fi

    for project in app slow; do
        [[ $("$tmp/$project/target/debug/$project") == "$project" ]]
        BUILD_TIMEOUT=10s client "$project" --rargo-daemon-stop
        rm -rf "$tmp/$project/target"
    done
    stop_server
    echo "both first builds succeeded with a $round store"
done
round=
