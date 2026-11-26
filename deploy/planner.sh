#!/bin/bash
# deploy/planner.sh SRC PLANNER BUILD
# Brings the planner checkout PLANNER (the cargo fork) to SRC/forks/cargo: BASE
# plus every patch there, applying the patches it does not have yet, and builds
# it against SRC/crates/rargo-plan in BUILD (a worktree of PLANNER), so that
# PLANNER itself stays clean. Prints the path of the built planner.
set -euo pipefail
src=$1 planner=$2 build=$3
stop() {
    echo "deploy/planner.sh: the planner checkout $planner $1" >&2
    echo "$2" >&2
    exit 1
}
forks=$src/forks/cargo
patches=("$forks"/*.patch)
[ -e "${patches[0]}" ] || { echo "deploy/planner.sh: $forks has no patches" >&2; exit 1; }
n=${#patches[@]}
[ -z "$(git -C "$planner" status --porcelain)" ] ||
    stop "is not clean" "Commit what belongs in the fork (and export it to forks/cargo with forks/export.sh) or discard the changes, then deploy again."
base=$(git -C "$planner" rev-parse -q --verify "$(cat "$forks/BASE")^{commit}") ||
    stop "does not have the commit $(cat "$forks/BASE") (forks/cargo/BASE)" "Fetch it (git -C $planner fetch origin), then deploy again."
git -C "$planner" merge-base --is-ancestor "$base" HEAD ||
    stop "is not based on $(cat "$forks/BASE") (forks/cargo/BASE)" "Check out BASE plus the patches in forks/cargo (forks/setup.sh shows how), then deploy again."
mapfile -t have < <(git -C "$planner" rev-list --reverse "$base..HEAD")
[ "${#have[@]}" -le "$n" ] ||
    stop "has ${#have[@]} commits on BASE, more than the $n patches in forks/cargo" "Export the fork's commits to forks/cargo (forks/export.sh) and release them, or reset the checkout to BASE plus the $n patches."
subject() {
    awk '/^$/ { exit } /^Subject: / { s = substr($0, 10); on = 1; next } on && /^[ \t]/ { s = s $0; next } { on = 0 }
        END { sub(/^\[PATCH[^]]*\] /, "", s); print s }' "$1"
}
for i in "${!have[@]}"; do
    [ "$(git -C "$planner" log -1 --format=%s "${have[$i]}")" = "$(subject "${patches[$i]}")" ] ||
        stop "has $(git -C "$planner" log -1 --format='%h "%s"' "${have[$i]}") where forks/cargo has $(basename "${patches[$i]}")" \
            "Reset the checkout to BASE plus the first $i patches in forks/cargo (the deploy applies the rest), or export the fork's commits (forks/export.sh) and release them."
done
if [ "${#have[@]}" -lt "$n" ]; then
    echo "applying $((n - ${#have[@]})) of the $n patches to the planner checkout" >&2
    am() { git -C "$planner" -c user.name=rargo -c user.email=noreply@rargo.invalid am "$@"; }
    am -q "${patches[@]:${#have[@]}}" || {
        am --abort || true
        stop "does not take $(basename "${patches[${#have[@]}]}") (git am failed on it, and nothing was applied)" "Rebase the patches onto the checkout by hand, export them (forks/export.sh) and release them."
    }
fi
head=$(git -C "$planner" rev-parse HEAD)
git -C "$planner" worktree prune
common() { git -C "$1" rev-parse --path-format=absolute --git-common-dir 2>/dev/null; }
if [ -d "$build/src" ] && [ "$(common "$build/src")" = "$(common "$planner")" ]; then
    git -C "$build/src" checkout -q --force --detach "$head"
else
    rm -rf "$build/src"
    mkdir -p "$build"
    git -C "$planner" worktree add -q --force --detach "$build/src" "$head"
fi
plan=$src/crates/rargo-plan
sed -i "s|^rargo-plan = { path = \"[^\"]*\" }|rargo-plan = { path = \"$plan\" }|" "$build/src/Cargo.toml"
grep -qxF "rargo-plan = { path = \"$plan\" }" "$build/src/Cargo.toml" ||
    stop "has no line rargo-plan = { path = \"...\" } in its Cargo.toml" "The first patch in forks/cargo should add the rargo-plan path dependency."
(cd "$build/src" && CFG_RELEASE_CHANNEL=stable CARGO_TARGET_DIR=$build/target cargo build --release --bin cargo >&2)
[ -z "$(git -C "$planner" status --porcelain)" ] || stop "was left changed by the build" "This is a bug in deploy/planner.sh."
echo "$build/target/release/cargo"
