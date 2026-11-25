#!/bin/sh
set -e
cd "$(dirname "$0")"
jar=${TLA2TOOLS:-$HOME/.local/share/tla/tla2tools.jar}
meta=$(mktemp -d)
trap 'rm -rf "$meta"' EXIT
java -XX:+UseParallelGC -cp "$jar" tlc2.TLC -deadlock -workers auto -metadir "$meta" -config "$2" "$1.tla"
