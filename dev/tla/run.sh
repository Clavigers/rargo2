#!/bin/sh
# Model-checks a spec with TLC: dev/tla/run.sh StoreSync StoreSync_TRUE.cfg
# Needs java and tla2tools.jar (TLA2TOOLS, default ~/.local/share/tla/tla2tools.jar).
set -e
cd "$(dirname "$0")"
jar=${TLA2TOOLS:-$HOME/.local/share/tla/tla2tools.jar}
meta=$(mktemp -d)
trap 'rm -rf "$meta"' EXIT
java -XX:+UseParallelGC -cp "$jar" tlc2.TLC -deadlock -workers auto -metadir "$meta" -config "$2" "$1.tla"
