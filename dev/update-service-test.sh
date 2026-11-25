#!/bin/bash
set -uo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
fail=0
run() { RARGO_WORK=$tmp/work RARGO_SERVICE=$tmp/service "$here/deploy/update-service.sh" "$@" >"$tmp/out" 2>&1; }
refused() {
    local what=$1; shift
    if run "$@"; then echo "FAIL $what: deployed"; fail=1; return; fi
    if ! grep -q 'dev/release.sh X.Y.Z' "$tmp/out"; then echo "FAIL $what: no hint"; cat "$tmp/out"; fail=1; return; fi
    if [ -e "$tmp/service" ]; then echo "FAIL $what: touched the service"; fail=1; return; fi
    echo "ok   $what: $(head -n 1 "$tmp/out")"
}
refused 'no argument'
refused 'origin/main' origin/main
refused 'a commit' "$(git -C "$here" rev-parse HEAD)"
refused 'a version without v' 0.6.5
refused 'an unpublished tag' v999.0.0
RARGO_RELEASE_REPO=cli/cli refused 'a release without the client' v2.0.0
latest=$(gh release view --repo sadosystems/rargo2 --json tagName --jq .tagName)
run "$latest"
if grep -q 'env.sh' "$tmp/out" && ! grep -q 'dev/release.sh X.Y.Z' "$tmp/out"; then
    echo "ok   the published release $latest is accepted"
else
    echo "FAIL the published release $latest: $(cat "$tmp/out")"; fail=1
fi
exit $fail
