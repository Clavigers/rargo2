#!/bin/bash
# deploy/release-local.sh points rargo.rs/dl/VERSION (what install.sh installs) at a release
# only once the build server runs it, and not at all when the deploy fails.
set -uo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
fail=0
repo=$tmp/repo bin=$tmp/bin dl=$tmp/data/dl
mkdir -p "$repo/deploy" "$repo/dev" "$bin" "$dl/0.0.1"
cp "$here/deploy/release-local.sh" "$repo/deploy/"
printf '#!/bin/sh\nexit 0\n' >"$repo/dev/ci-passed.sh"
printf '#!/bin/sh\ncat "%s/VERSION" >"%s/seen"; readlink "%s/latest" >>"%s/seen"; exit "${SERVICE_RC:-0}"\n' "$dl" "$tmp" "$dl" "$tmp" >"$repo/deploy/update-service.sh"
printf '#!/bin/sh\nexit 0\n' >"$repo/deploy/update-site.sh"
cat >"$bin/gh" <<'GH'
#!/bin/sh
[ "$1 $2" = "release download" ] || exit 1
while [ $# -gt 0 ]; do [ "$1" = --dir ] && dir=$2; shift; done
echo client >"$dir/rargo-x86_64-unknown-linux-musl"
(cd "$dir" && sha256sum rargo-x86_64-unknown-linux-musl >SHA256SUMS)
GH
chmod +x "$repo"/deploy/*.sh "$repo/dev/ci-passed.sh" "$bin/gh"
git -C "$repo" init -q && git -C "$repo" -c user.name=t -c user.email=t@t.invalid commit -q --allow-empty -m t
git -C "$repo" tag v0.0.2 && git -C "$repo" tag v0.0.3
echo 0.0.1 >"$dl/VERSION"; ln -s 0.0.1 "$dl/latest"
release() { PATH=$bin:$PATH RARGO_SITE_DATA=$tmp/data "$repo/deploy/release-local.sh" "$@" >"$tmp/out" 2>&1; }
check() { if [ "$2" = "$3" ]; then echo "ok   $1"; else echo "FAIL $1: '$2', not '$3'"; cat "$tmp/out"; fail=1; fi; }

release v0.0.2
check 'during the deploy, VERSION and latest still name the running release' "$(tr '\n' ' ' <"$tmp/seen")" '0.0.1 0.0.1 '
check 'after the deploy, VERSION names the new release' "$(cat "$dl/VERSION")" 0.0.2
check 'after the deploy, latest names the new release' "$(readlink "$dl/latest")" 0.0.2

SERVICE_RC=1 release v0.0.3 && { echo "FAIL a failed deploy: release-local.sh succeeded"; fail=1; }
check 'a failed deploy leaves VERSION at the running release' "$(cat "$dl/VERSION")" 0.0.2
check 'a failed deploy leaves latest at the running release' "$(readlink "$dl/latest")" 0.0.2
exit $fail
