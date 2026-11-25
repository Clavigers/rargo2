#!/bin/bash
# The gate's time ratchet. This check is allowed to depend on wall-clock
# time (an exception to "nothing in the gate waits on or measures time").
#   dev/gate-budget.sh raise-ok
#       exit 1 if dev/gate-budget's total is higher than on origin/main
#       and no commit since says `Gate-budget-raise: <why>`.
#   dev/gate-budget.sh check ELAPSED GATE_DIR SUITE...
#       exit 1, with a message, if ELAPSED is over the total. The message
#       names each suite's time against its recorded time.
set -euo pipefail
cd "$(dirname "$0")/.."
file=dev/gate-budget
value() { awk -v k="$2" '$1 == k { print $2 }' "$1"; }
case ${1:-} in
raise-ok)
    new=$(value "$file" total)
    old=$(git show origin/main:"$file" 2>/dev/null | awk '$1 == "total" { print $2 }' || true)
    [ -z "$old" ] || [ "$new" -le "$old" ] && exit 0
    git log --format=%B origin/main..HEAD | grep -q '^Gate-budget-raise: .\+' && exit 0
    echo "gate budget: total $new is above origin/main's $old; add a commit line 'Gate-budget-raise: <why>'" >&2
    exit 1
    ;;
check)
    elapsed=$2 dir=$3
    shift 3
    total=$(value "$file" total)
    [ "$elapsed" -le "$total" ] && exit 0
    echo "gate budget: ${elapsed}s is over the ${total}s budget in $file" >&2
    for s in "$@"; do
        took=$(cat "$dir/$s.secs" 2>/dev/null || echo "?")
        was=$(value "$file" "$s")
        mark=""
        [ "$took" != "?" ] && [ -n "$was" ] && [ "$took" -gt "$was" ] && mark="  +$((took - was))s"
        printf '  %-16s %4ss (recorded %ss)%s\n' "$s" "$took" "${was:-?}" "$mark" >&2
    done
    exit 1
    ;;
*)
    echo "usage: $0 raise-ok | check ELAPSED GATE_DIR SUITE..." >&2
    exit 2
    ;;
esac
