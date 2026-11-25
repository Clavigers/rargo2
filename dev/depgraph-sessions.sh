#!/bin/bash
# Many incremental sessions of regex-syntax with a compiler (#114): the edit
# of depgraph-share.sh goes in and out N times. Prints each session's index
# space and record count, then checks the last incremental build's rmeta
# against a clean build of the same source.
#
#   dev/depgraph-sessions.sh RUSTC OUT_DIR [N]
set -euo pipefail
rustc=$1 out=$2 n=${3:-40}
src=${REGEX_SYNTAX:-/root/rargo-work/cargo-home/registry/src/index.crates.io-1949cf8c6b5b557f/regex-syntax-0.8.12}
rm -rf "$out"; mkdir -p "$out/target" "$out/clean"
cp -a "$src" "$out/src-crate"
cd "$out/src-crate"
build() { # incremental dir, output dir
    "$rustc" --crate-name regex_syntax --edition 2021 src/lib.rs --crate-type lib \
        --emit=metadata,link -C embed-bitcode=no -C debuginfo=2 \
        --cfg 'feature="default"' --cfg 'feature="std"' --cfg 'feature="unicode"' \
        --cfg 'feature="unicode-age"' --cfg 'feature="unicode-bool"' --cfg 'feature="unicode-case"' \
        --cfg 'feature="unicode-gencat"' --cfg 'feature="unicode-perl"' --cfg 'feature="unicode-script"' \
        --cfg 'feature="unicode-segment"' -C metadata=0123456789abcdef \
        ${1:+-C incremental="$1"} --out-dir "$2" --cap-lints allow
}
for i in $(seq 1 "$n"); do
    if [ $((i % 2)) = 0 ]; then
        sed -i 's|        let ast = self.ast.parse(pattern)?;|        let ast = self.ast.parse(pattern)?;\n        std::hint::black_box(pattern.len());|' src/parser.rs
    else
        sed -i '/black_box(pattern.len())/d' src/parser.rs
    fi
    build "$out/incr" "$out/target"
    g="$(ls -dt "$out"/incr/regex_syntax-*/s-* | head -1)/dep-graph.bin"
    python3 -I -c '
import struct, sys
d = open(sys.argv[1], "rb").read()[: -len(b"rust-end-file")]
m, c, e = struct.unpack("<QQQ", d[-24:])
print(f"session {sys.argv[2]:>3}: index space {m:>7}, nodes {c:>7}, unused {m - c:>6}")' "$g" "$i"
done
build "" "$out/clean"
cmp "$out/target/libregex_syntax.rmeta" "$out/clean/libregex_syntax.rmeta" && echo "rmeta: incremental build after $n sessions = clean build"
