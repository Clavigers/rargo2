#!/bin/bash
# Cut and deploy the version on main, or resume the same tagged release.
set -Eeuo pipefail
step=setup
trap 'echo "Release failed during $step. Run dev/release.sh again for this version to resume." >&2' ERR
die() { echo "$step: $*" >&2; exit 1; }

version=${1:-}
[[ "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] \
    || die 'usage: dev/release.sh X.Y.Z'
tag=v$version
project=sadosystems/rargo2
work=${RARGO_WORK:-/root/rargo-work}
repo=$(cd "$(dirname "$0")/.." && pwd)
cd "$repo"
mkdir -p "$work"
exec 9>"$work/release.lock"
flock -n 9 || die 'another release is running on this machine'

step='selecting the release commit'
[ "$(git branch --show-current)" = main ] || die 'check out main'
[ -z "$(git status --porcelain)" ] || die 'main must be clean'
git fetch -q origin main --tags
remote_tag=$(git ls-remote --tags origin "refs/tags/$tag")
if [ -n "$remote_tag" ]; then
    commit=$(git rev-parse "$tag^{commit}")
    git merge-base --is-ancestor "$commit" origin/main || die "$tag must be on main"
else
    commit=$(git rev-parse HEAD)
    [ "$(git rev-parse origin/main)" = "$commit" ] || die 'main must match origin/main'
fi
manifest=$(git show "$commit:Cargo.toml" | sed -n 's/^version = "\([^"]*\)"$/\1/p' | head -n 1)
[ "$manifest" = "$version" ] || die "Cargo.toml at $commit is at $manifest"
notes=$(git show "$commit:CHANGELOG.md" | awk -v v="$version" '$0 == "## " v {on=1; next} /^## / {on=0} on && NF')
[ -n "$notes" ] || die "CHANGELOG.md has no notes for $version"
issues=$(printf '%s\n' "$notes" | { grep -oE '#[0-9]+' || true; } | cut -c2- | sort -un)

if [ -z "$remote_tag" ]; then
    step='running the build-machine gate'
    # The suites run their own servers: a token or server meant for the
    # service check below would reach their clients.
    env -u RARGO_TOKEN -u RARGO_SERVER dev/gate.sh
    step='waiting for the successful gate status'
    dev/release-gate.sh "$commit" --wait
    step='creating and pushing the tag'
    if git show-ref --verify --quiet "refs/tags/$tag"; then
        [ "$(git rev-parse "$tag^{commit}")" = "$commit" ] || die "$tag points at a different commit"
    else
        git tag -a "$tag" "$commit" -m "rargo $version"
    fi
    # Repeating a push of the same tag is safe even if GitHub accepted
    # the first request before returning an error.
    for attempt in 1 2 3; do
        if git push origin "refs/tags/$tag"; then
            break
        fi
        [ "$attempt" -lt 3 ] || die "could not push $tag after $attempt attempts"
        sleep 2
    done
else
    step='checking the tagged commit gate'
    dev/release-gate.sh "$commit"
fi

step='waiting for the release workflow'
run=
for _ in $(seq 1 60); do
    run=$(gh run list -R "$project" --workflow release.yml --event push --branch "$tag" \
        --commit "$commit" --limit 1 --json databaseId --jq '.[0].databaseId // ""')
    [ -z "$run" ] || break
    sleep 5
done
[ -n "$run" ] || die "no release workflow appeared for $tag"
if [ -n "$remote_tag" ]; then
    IFS=$'\t' read -r attempt status conclusion < <(
        gh run view "$run" -R "$project" --json attempt,status,conclusion \
            --jq '[.attempt,.status,.conclusion] | @tsv'
    )
    if [ "$status" = completed ] && [ "$conclusion" != success ]; then
        gh run rerun "$run" -R "$project" --failed
        for _ in $(seq 1 60); do
            current=$(gh run view "$run" -R "$project" --json attempt --jq .attempt)
            [ "$current" -le "$attempt" ] || break
            sleep 5
        done
        [ "$current" -gt "$attempt" ] || die "the retry did not start for workflow $run"
    fi
fi
gh run watch "$run" -R "$project" --exit-status --interval 20

step='deploying the tagged release'
state=$work/release-$tag
mkdir -p "$state"
checkout=$(mktemp -d "$state/checkout.XXXXXXXX")
cleanup() {
    git worktree remove "$checkout" >/dev/null 2>&1 || true
    rmdir "$checkout" 2>/dev/null || true
}
trap cleanup EXIT
git worktree add -q --detach "$checkout" "$commit"
if [ ! -f "$state/deployed-$commit" ]; then
    "$checkout/deploy/release-local.sh" "$tag"
    touch "$state/deployed-$commit"
fi

step='checking the updated service over UDP and TCP'
RARGO_SERVER=rargo.rs "$checkout/deploy/check-service.sh" /usr/local/bin/rargo

step='recording the release on its issues'
url="https://github.com/$project/releases/tag/$tag"
marker="<!-- rargo-release $tag -->"
for issue in $issues; do
    comments=$(gh api --paginate "repos/$project/issues/$issue/comments" --jq '.[].body')
    if ! grep -Fx "$marker" <<<"$comments" >/dev/null; then
        gh issue comment "$issue" -R "$project" --body "Released in [rargo $tag]($url)."$'\n\n'"$marker"
    fi
done
printf 'Released and checked %s: %s\n' "$tag" "$url"
