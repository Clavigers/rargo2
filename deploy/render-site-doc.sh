#!/bin/bash
# Render Markdown from a release with the site's template and assets.
set -euo pipefail
input=$(realpath -- "${1:?release Markdown}")
output=$(realpath -m -- "${2:?output HTML}")
site=${RARGO_SITE_REPO:-/root/rargo-hq/deploy.git}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
git --git-dir="$site" archive main flake.nix flake.lock src/demo/template.html \
    site/index.css site/index.js site/src/demo/demo.css | tar -x -C "$tmp"
cd "$tmp"
version() { sha256sum "$1" | cut -c1-12; }
systemd-run --scope -q -p CPUQuota=200% -p MemoryMax=8G \
    nix --extra-experimental-features 'nix-command flakes' develop --command \
    pandoc --toc --toc-depth=2 --standalone -M document-css=false \
    --css "/index.css?v=$(version site/index.css)" \
    --css "/src/demo/demo.css?v=$(version site/src/demo/demo.css)" \
    -V 'header-includes=<script src="/index.js?v='"$(version site/index.js)"'"></script>' \
    --template=src/demo/template.html "$input" >"$output"
