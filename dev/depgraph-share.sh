#!/bin/bash
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
