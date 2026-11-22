#!/bin/sh
# Runs the 20-agent swarm benchmark for one repository in both modes.
#   dev/bench-swarm.sh ra|ruff|bevy [agents]
# Results are appended to $RARGO_WORK/bench/results.jsonl.
set -eu
work=${RARGO_WORK:-/root/rargo-work}
repo=$(cd "$(dirname "$0")/.." && pwd)
name=$1
agents=${2:-20}
. "$work/env.sh"
out=$work/bench/results.jsonl
# rargo: fresh daemon and store, so nothing is warm.
"$repo/dev/rargod.sh" stop >/dev/null 2>&1 || true
rm -rf "$work/state"
"$repo/dev/rargod.sh" start >/dev/null
RARGO_SOCKET=$work/state/rargod.sock python3 "$repo/dev/swarm.py" --repo "$name" --mode rargo --agents "$agents" --out "$out"
"$repo/dev/rargod.sh" stop >/dev/null 2>&1 || true
# stock: same CPU and memory cap as the daemon gets.
systemd-run --scope -q -p CPUQuota=2400% -p MemoryMax=90G \
    python3 "$repo/dev/swarm.py" --repo "$name" --mode stock --agents "$agents" --out "$out"
