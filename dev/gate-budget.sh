#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."
file=dev/gate-budget
hard=300
value() { awk -v k="$2" '$1 == k { print $2 }' "$1"; }
case ${1:-} in
raise-ok)
    new=$(value "$file" total)
    if [ "$new" -gt "$hard" ]; then
        echo "gate budget: total $new is over the hard limit of ${hard}s" >&2
        exit 1
    fi
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
    [ "$total" -le "$hard" ] || total=$hard
    [ "$elapsed" -le "$total" ] && exit 0
    if [ -f "$dir/recorded.secs" ]; then
        recorded=$dir/recorded.secs from="the last gate in $dir that passed"
    else
        recorded=$file from="the reference times in $file"
    fi
    echo "gate budget: ${elapsed}s is over the ${total}s budget in $file; each step against $from:" >&2
    for s in "$@"; do
        took=$(cat "$dir/$s.secs" 2>/dev/null || echo "?")
        was=$(value "$recorded" "$s")
        mark=""
        [ "$took" != "?" ] && [ -n "$was" ] && [ "$took" -gt "$was" ] && mark="  +$((took - was))s"
        printf '  %-16s %4ss (recorded %ss)%s\n' "$s" "$took" "${was:-?}" "$mark" >&2
    done
    exit 1
    ;;
record)
    dir=$2
    shift 2
    for s in "$@"; do
        echo "$s $(cat "$dir/$s.secs")"
    done >"$dir/recorded.secs.tmp"
    mv "$dir/recorded.secs.tmp" "$dir/recorded.secs"
    ;;
*)
    echo "usage: $0 raise-ok | check ELAPSED GATE_DIR STEP... | record GATE_DIR STEP..." >&2
    exit 2
    ;;
esac
