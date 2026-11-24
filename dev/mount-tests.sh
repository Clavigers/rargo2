#!/bin/bash
# The tests that mount and make namespaces (fusewire's, the engine's): the
# build server compiles them, this machine runs them, as root. Nothing that
# is built is built here.
#
#   dev/mount-tests.sh [--release]
set -u
cd "$(dirname "$0")/.." || exit 2
out=$(rargo test --locked --no-run "$@" -p fusewire --test auto_unmount -p rargo-core --test engine 2>&1) || { echo "$out"; exit 1; }
# The engine starts the sandbox helper, which it finds beside itself.
rargo build --locked "$@" -p rargo-core || exit 1
tests=$(grep -o '(target/[^)]*)' <<<"$out" | tr -d '()')
[ -n "$tests" ] || { echo "no test programs in:"; echo "$out"; exit 1; }
asroot() { if [ "$(id -u)" = 0 ]; then "$@"; else sudo "$@"; fi; }
fail=0
for t in $tests; do
    echo "== $t"
    asroot "./$t" || fail=1
done
exit $fail
