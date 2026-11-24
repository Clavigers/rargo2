#!/bin/bash
# Lands the current branch through a pull request that merges itself when
# its checks pass (issue 4). Nobody reviews it; the checks are the gate.
#
#   dev/pr-land.sh "Title" BODY-FILE        (the body says: Closes #N)
#
# `gh pr merge --auto` waits for checks only where the branch has required
# checks, which this repository's plan does not offer while it is private.
# So this script does the waiting: it watches the PR's checks and merges
# only if every one passed.
set -euo pipefail
title=$1; body=$2
branch=$(git rev-parse --abbrev-ref HEAD)
[ "$branch" != main ] || { echo "on main: make a branch first" >&2; exit 1; }
[ -z "$(git status --porcelain)" ] || { echo "working tree is dirty" >&2; exit 1; }
commit=$(git rev-parse HEAD)
git fetch -q origin main
# Only prose-only changes bypass the machine suites.
run_gate=false
while IFS= read -r -d '' path; do
    case "$path" in
        *.md) ;;
        *) run_gate=true ;;
    esac
done < <(git diff --name-only --no-renames -z origin/main..."$commit")
git push -q -u origin "$branch"
url=$(gh pr list --state open --base main --head "$branch" --json url --jq 'first.url // ""')
if [ -z "$url" ]; then
    url=$(gh pr create --title "$title" --body-file "$body" --base main --head "$branch")
fi
echo "$url"
if "$run_gate" && ! "$(dirname "$0")/gate.sh"; then
    echo "gate failed; not merged: $url" >&2
    exit 1
fi
# Checks take a moment to be registered.
for _ in $(seq 1 30); do
    [ "$(gh pr checks "$url" --json name -q 'map(select(.name == "build-and-test" or .name == "new-user-path")) | length' 2>/dev/null || echo 0)" -eq 2 ] && break
    sleep 5
done
if [ "$(gh pr checks "$url" --json name -q 'map(select(.name == "build-and-test" or .name == "new-user-path")) | length')" -ne 2 ]; then
    echo "CI checks did not register: $url" >&2
    exit 1
fi
if gh pr checks "$url" --watch --fail-fast --interval 20 >/dev/null; then
    gh pr merge "$url" --squash --match-head-commit "$commit"
    git push -q origin ":$branch" 2>/dev/null || true
    # In the main checkout: back on main. In a second worktree (main is
    # checked out elsewhere): at what was merged.
    git fetch -q origin
    if git checkout -q main 2>/dev/null; then git pull -q --ff-only origin main; else git checkout -q --detach origin/main; fi
    echo "merged: $url"
else
    gh pr checks "$url" || true
    echo "checks failed; not merged: $url" >&2
    exit 1
fi
