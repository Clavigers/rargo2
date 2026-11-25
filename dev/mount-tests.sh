#!/bin/bash
set -u
cd "$(dirname "$0")/.." || exit 2
out=$(rargo test --locked --no-run "$@" -p fusewire --test auto_unmount -p rargo-core --test engine 2>&1) || { echo "$out"; exit 1; }
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
