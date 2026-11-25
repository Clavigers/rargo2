#!/bin/bash
set -euo pipefail
title=$1; body=$2
branch=$(git rev-parse --abbrev-ref HEAD)
[ "$branch" != main ] || { echo "on main: make a branch first" >&2; exit 1; }
[ -z "$(git status --porcelain)" ] || { echo "working tree is dirty" >&2; exit 1; }
commit=$(git rev-parse HEAD)
git push -q -u origin "$branch"
url=$(gh pr list --state open --base main --head "$branch" --json url --jq 'first.url // ""')
if [ -z "$url" ]; then
    url=$(gh pr create --title "$title" --body-file "$body" --base main --head "$branch")
fi
echo "$url"
for _ in $(seq 1 30); do
    [ "$(gh pr checks "$url" --json name -q 'map(select(.name == "build-and-test")) | length' 2>/dev/null || echo 0)" -eq 1 ] && break
    sleep 5
done
if [ "$(gh pr checks "$url" --json name -q 'map(select(.name == "build-and-test")) | length')" -ne 1 ]; then
    echo "CI checks did not register: $url" >&2
    exit 1
fi
if gh pr checks "$url" --watch --fail-fast --interval 20 >/dev/null; then
    gh pr merge "$url" --squash --match-head-commit "$commit"
    git push -q origin ":$branch" 2>/dev/null || true
    git fetch -q origin
    if git checkout -q main 2>/dev/null; then git pull -q --ff-only origin main; else git checkout -q --detach origin/main; fi
    echo "merged: $url"
else
    gh pr checks "$url" || true
    echo "checks failed; not merged: $url" >&2
    exit 1
fi
