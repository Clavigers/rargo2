#!/bin/sh
set -eu
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cp "$here/../../../../nix/default/flake.nix" "$here/../../../../nix/default/flake.lock" "$work/"
cd "$work"
nix --extra-experimental-features 'nix-command flakes' print-dev-env --json --no-write-lock-file .#default >"$here/print-dev-env.json"
python3 -I - "$here/print-dev-env.json" >"$work/roots" <<'PY'
import json, re, sys
seen = set()
for v in json.load(open(sys.argv[1]))["variables"].values():
    if v.get("type") == "exported" and isinstance(v.get("value"), str):
        for m in re.finditer(r"/nix/store/([A-Za-z0-9\-_.+?=]*)", v["value"]):
            n = m.group(1)
            if len(n) > 33 and n[32] == "-":
                seen.add(n)
print("\n".join("/nix/store/" + n for n in sorted(seen)))
PY
xargs nix-store -qR <"$work/roots" >"$here/closure.txt"
