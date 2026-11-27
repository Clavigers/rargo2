#!/usr/bin/env python3
"""Lower crates/rargo-purity/allow.txt to what the code has now. Never raises a count: prints those instead."""
import subprocess, sys, collections, re
root = sys.argv[1]
out = subprocess.run(["cargo", "run", "-q", "-p", "rargo-purity", "--example", "findings"], cwd=root, capture_output=True, text=True, check=True).stdout
have = collections.Counter()
for line in out.splitlines():
    check, where, _ = line.split("\t", 2)
    have[(check, where.rsplit(":", 1)[0])] += 1
path = f"{root}/crates/rargo-purity/allow.txt"
kept, listed, raised = [], set(), []
for line in open(path).read().splitlines():
    if not line.strip():
        continue
    check, file, count, reason = line.split("\t", 3)
    listed.add((check, file))
    n = have[(check, file)]
    if n == 0:
        continue
    if n > int(count):
        raised.append(f"{check} {file}: {count} -> {n}")
        n = int(count)
    if n < int(count):
        reason = re.sub(r":[^:]*\(\d+\).*$", "", reason)
    kept.append(f"{check}\t{file}\t{n}\t{reason}")
new = [f"{c} {f}: 0 -> {n}" for (c, f), n in sorted(have.items()) if (c, f) not in listed]
open(path, "w").write("\n".join(kept) + "\n")
for r in raised + new:
    print("RAISED:", r)
print(len(kept), "entries kept")
