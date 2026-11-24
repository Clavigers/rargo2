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
git push -q -u origin "$branch"
url=$(gh pr create --title "$title" --body-file "$body" --base main --head "$branch")
echo "$url"
# Checks take a moment to be registered.
for _ in $(seq 1 30); do
    [ "$(gh pr checks "$url" --json name -q length 2>/dev/null || echo 0)" -gt 0 ] && break
    sleep 5
done
if gh pr checks "$url" --watch --fail-fast --interval 20 >/dev/null; then
    gh pr merge "$url" --squash --delete-branch
    git checkout -q main && git pull -q --ff-only origin main
    echo "merged: $url"
else
    gh pr checks "$url" || true
    echo "checks failed; not merged: $url" >&2
    exit 1
fi
