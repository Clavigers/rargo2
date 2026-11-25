#!/bin/sh
set -eu
out=$(realpath "$1")
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
export HOME="$work/home" GIT_CONFIG_NOSYSTEM=1 GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
mkdir -p "$HOME"
git config --global init.defaultBranch main
git config --global protocol.file.allow always
git config --global gc.auto 0
day=0
commit() {
    day=$((day + 1))
    GIT_AUTHOR_DATE="2026-01-0${day}T00:00:00Z" GIT_COMMITTER_DATE="2026-01-0${day}T00:00:00Z" git commit -q "$@"
}
day2() {
    day=$((day + 1))
    GIT_AUTHOR_DATE="2026-02-$((10 + day))T00:00:00Z" GIT_COMMITTER_DATE="2026-02-$((10 + day))T00:00:00Z" git "$@"
}

mkdir "$work/module" && cd "$work/module"
git init -q && echo module > m && git add m && commit -m module

mkdir "$work/fx" && cd "$work/fx"
git init -q
mkdir -p src tools
seq 1 3000 > src/big.txt
printf 'pub fn a() {}\n' > src/lib.rs
printf '#!/bin/sh\necho hi\n' > tools/run.sh && chmod +x tools/run.sh
ln -s src/lib.rs link
printf '*.log\nbuild/\nemptyign/\n/top-only\nkeep/\n!keep/re.log\nonlyign/*.tmp\nnest-ign/\ntracked-ign/\n[a-c]?.txt\n**/deep2/**\n\\#hash\n' > .gitignore
git add -A && commit -m one
git tag -a v0.1.0 -m "first release"
seq 1 3001 > src/big.txt && git add -A && commit -m two
git -c repack.useDeltaBaseOffset=false repack -q -a -d -f
seq 1 3002 > src/big.txt && git add -A && commit -m three
git checkout -q -b side HEAD~1
printf 'side\n' > side.txt && git add -A && commit -m "side one"
git tag side-light
git checkout -q main
day2 merge -q --no-ff -m "merge side" side
git submodule -q add "$work/module" nested/module
day2 commit -q -m "add module"
git tag -a v0.2.0 -m "second release"
git tag -a v0.2.0-again -m "tag of a tag" v0.2.0
git tag tree-tag HEAD^{tree}
git repack -q -d
seq 1 3003 > src/big.txt && git add -A && day2 commit -q -m four
git pack-refs --all
git branch feature/x
git worktree add -q ../wt feature/x
git worktree add -q --detach ../wt-detached HEAD~1
(cd ../wt && git update-index --index-version 4 && echo wt > wt.txt && git add wt.txt)
(cd ../wt-detached && git update-index --split-index && echo d > d.txt && git add d.txt)
printf 'pub fn b() {}\n' > src/lib.rs
git update-ref refs/tags/loose-tag HEAD~1
printf 'excluded-by-info\n' >> .git/info/exclude
git clone -q --depth 2 "file://$work/fx" ../shallow

mkdir -p allign mixed tracked-ign/sub emptyign build/deep keep onlyign nest nest-ign deep/a/b emptyuntracked a/emptysub b/build/q c d e/f m/deep2/n
touch allign/a.log mixed/a.log mixed/b.rs tracked-ign/u.log tracked-ign/sub/v.rs build/deep/x top-only sub-top-only keep/re.log onlyign/a.tmp deep/a/b/z.log a/x.log b/build/q/y c/z.log e/f/w.log m/deep2/n/o '#hash' b1.txt d2.txt excluded-by-info sub.log
printf 't\n' > tracked-ign/t.rs && git add -f tracked-ign/t.rs
(cd nest && git init -q && touch inner.log)
(cd nest-ign && git init -q)
ln -s ../a d/link.log
ln -s a dirlink

expect() {
    name=$1; shift
    (cd "$work/fx" && "$@") > "$work/expect-$name"
}
expect ignored git ls-files -z -o -i --exclude-standard --directory
expect head git rev-parse HEAD
expect short git rev-parse --short HEAD
expect points-at git for-each-ref "--format=%(objectname) %(objecttype) %(*objectname) %(refname)" --points-at=HEAD~1 refs/tags
expect merged git for-each-ref "--format=%(objectname) %(objecttype) %(*objectname) %(refname)" --merged=HEAD refs/tags
expect first-parent git rev-list --first-parent HEAD
expect range git rev-list HEAD~1 ^HEAD~3
expect objects git rev-list --objects --no-object-names --filter=blob:none --no-walk HEAD
expect ls-tree git ls-tree -r -z HEAD
expect all-objects git cat-file --batch-all-objects --batch-check
expect stage git ls-files -s
expect wt-stage sh -c 'cd ../wt && git ls-files -s'
expect wt-detached-stage sh -c 'cd ../wt-detached && git ls-files -s'
expect wt-head sh -c 'cd ../wt && git rev-parse HEAD'
expect wt-detached-head sh -c 'cd ../wt-detached && git rev-parse HEAD'
expect shallow-first-parent sh -c 'cd ../shallow && git rev-list --first-parent HEAD'
expect module-head sh -c 'cd nested/module && git rev-parse HEAD'

printf 'gitdir: ../fx/.git/worktrees/wt\n' > "$work/wt/.git"
printf 'gitdir: ../fx/.git/worktrees/wt-detached\n' > "$work/wt-detached/.git"
cd "$work"
: > "$out"
: > "$work/empty"
record() {
    kind=$1 path=$2 file=$3
    printf '%s%s\0%s\0' "$kind" "$path" "$(wc -c < "$file" | tr -d ' ')" >> "$out"
    cat "$file" >> "$out"
}
find fx wt wt-detached shallow \( -path '*/.git/hooks' -o -path '*/.git/logs' -o -path '*/hooks' \) -prune -o -print | LC_ALL=C sort | while read -r p; do
    if [ -L "$p" ]; then
        readlink "$p" > "$work/link-target"
        printf '%s' "$(cat "$work/link-target")" > "$work/link-target"
        record l "$p" "$work/link-target"
    elif [ -d "$p" ]; then
        if [ -z "$(ls -A "$p")" ]; then record d "$p" "$work/empty"; fi
    else
        case "$p" in
        */.git/* | */.gitignore | */.git) record f "$p" "$p" ;;
        *) record f "$p" "$work/empty" ;;
        esac
    fi
done
for e in expect-*; do record x "${e#expect-}" "$e"; done
