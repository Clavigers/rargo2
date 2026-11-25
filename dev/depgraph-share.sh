#!/bin/bash
# How much of regex-syntax's dep-graph.bin survives a one-statement edit, in
# rargo's chunks (#114): build it incrementally, edit one statement in
# Parser::parse, build again, and compare the two dep-graph.bin files with
# the chunkshare example. Then once more, a third session after the same
# edit is reverted.
#
#   dev/depgraph-share.sh RUSTC CHUNKSHARE OUT_DIR
# (CHUNKSHARE: the chunkshare example of rargo-core, built with
# `rargo build --release -p rargo-core --example chunkshare`.)
set -euo pipefail
rustc=$1 share=$2 out=$3
src=${REGEX_SYNTAX:-/root/rargo-work/cargo-home/registry/src/index.crates.io-1949cf8c6b5b557f/regex-syntax-0.8.12}
rm -rf "$out"; mkdir -p "$out"
cp -a "$src" "$out/src-crate"
mkdir -p "$out/target"
cd "$out/src-crate"
build() {
    "$rustc" --crate-name regex_syntax --edition 2021 src/lib.rs --crate-type lib \
        --emit=dep-info,metadata,link -C embed-bitcode=no -C debuginfo=2 \
        --cfg 'feature="default"' --cfg 'feature="std"' --cfg 'feature="unicode"' \
        --cfg 'feature="unicode-age"' --cfg 'feature="unicode-bool"' --cfg 'feature="unicode-case"' \
        --cfg 'feature="unicode-gencat"' --cfg 'feature="unicode-perl"' --cfg 'feature="unicode-script"' \
        --cfg 'feature="unicode-segment"' -C metadata=0123456789abcdef \
        -C incremental="$out/incr" --out-dir "$out/target" --cap-lints allow
    # The session directory just finished is the newest one.
    cp "$(ls -dt "$out"/incr/regex_syntax-*/s-* | head -1)/dep-graph.bin" "$out/dep-graph.$1.bin"
}
build 1
sed -i 's|        let ast = self.ast.parse(pattern)?;|        let ast = self.ast.parse(pattern)?;\n        std::hint::black_box(pattern.len());|' src/parser.rs
grep -q 'black_box(pattern.len())' src/parser.rs
build 2
sed -i '/black_box(pattern.len())/d' src/parser.rs
build 3
ls -l "$out"/dep-graph.*.bin | awk '{print $5, $9}'
echo "edit:   $("$share" "$out/dep-graph.1.bin" "$out/dep-graph.2.bin")"
echo "revert: $("$share" "$out/dep-graph.2.bin" "$out/dep-graph.3.bin")"
