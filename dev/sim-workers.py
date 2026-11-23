#!/usr/bin/env python3
"""What is the best a worker could do for a cold build?

    dev/sim-workers.py target/cargo-timings/cargo-timing.html [SHAPE ...]

Replays the units of a build that was run with `--timings` (their measured
frontend time, CPU time and dependencies come from the report's data block)
on machines of other shapes. A SHAPE is core counts joined by `+`: `8` is one
machine with 8 cores, `4+4` a server with 4 and a worker with 4.

The model is the one rargo schedules by:

- a unit runs on one machine from start to finish;
- every running unit holds one core; while it generates code it also takes
  cores nothing else on its machine wants, up to `--threads` (rustc's helper
  threads and the shared jobserver);
- a unit may start when the metadata of the libraries it depends on is ready
  (when they finished, for anything that links);
- the network is free: sending inputs and bringing results home take no time,
  and a unit goes to whichever machine has a core.

So the figure for `4+4` is a lower bound for a server with one worker: what
remains of the distance to `8` when the link costs nothing is what whole-unit
dispatch cannot give, because a core on another machine cannot help generate
code for a unit that runs here.
"""
import argparse
import json
import re
import sys


def load(path):
    text = open(path, encoding="utf-8").read()
    m = re.search(r'<script type="application/json" id="rargo-timings">(.*?)</script>', text, re.S)
    if not m:
        sys.exit(f"{path}: no timings data block (was it written by rargo --timings?)")
    return json.loads(m.group(1))


class Unit:
    def __init__(self, u, threads):
        self.i = u["i"]
        self.name = u["name"]
        self.deps = [(j, bool(m)) for j, m in u["deps"]]
        self.needs_objects = u["needs_objects"]
        self.runs = u["runs"] and u["end"] is not None
        wall, cpu = u["wall"], u["cpu"]
        self.pipelined = False
        self.front = 0.0   # serial seconds before the metadata is ready
        self.work = 0.0    # core-seconds of code generation after that
        self.tail = 0.0    # serial seconds at the end (archiving, linking)
        if not self.runs:
            return
        began = max(u["start"], u["end"] - wall)
        if u["meta"] is not None and began <= u["meta"] <= u["end"]:
            self.pipelined = True
            self.front = max(0.001, u["meta"] - began)
            self.work = max(0.0, cpu - self.front)
        else:
            # No metadata announced (a binary, a test, a build script, a proc
            # macro): split into a serial part and parallel work such that
            # wall = serial + work / threads and cpu = serial + work.
            if cpu <= wall or threads <= 1:
                self.front = wall
            else:
                serial = min(wall, max(0.0, (wall * threads - cpu) / (threads - 1)))
                self.front = max(0.001, serial)
                self.work = max(0.0, cpu - serial)
        self.measured_wall = wall


def closure(units):
    """For units that link: everything below them, not only direct deps."""
    memo = {}

    def below(i):
        if i in memo:
            return memo[i]
        memo[i] = set()
        out = set()
        for j, _ in units[i].deps:
            if j in units:
                out.add(j)
                out |= below(j)
        memo[i] = out
        return out

    return {i: below(i) for i in units}


def simulate(units, machines, threads, order, place, dt=0.01):
    """Returns the wall time of the build on `machines` (a list of core counts)."""
    below = closure(units)
    # Longest path to the end of the build, for choosing what starts first.
    level = {}

    def bottom(i):
        if i in level:
            return level[i]
        level[i] = 0.0
        u = units[i]
        own = u.front + u.work / threads if u.runs else 0.0
        level[i] = own + max((bottom(k) for k in dependents.get(i, [])), default=0.0)
        return level[i]

    dependents = {}
    for u in units.values():
        for j, _ in u.deps:
            dependents.setdefault(j, []).append(u.i)
    for i in units:
        bottom(i)

    meta_at, done_at = {}, {}
    state = {}  # i -> [machine, phase, remaining]; phase 0 frontend, 1 codegen
    waiting = set(units)
    t = 0.0

    def ready(u):
        for j, meta_ok in u.deps:
            if j not in units:
                continue
            d = units[j]
            if meta_ok and d.pipelined:
                if j not in meta_at:
                    return False
            elif j not in done_at:
                return False
        if u.needs_objects and any(j not in done_at for j in below[u.i]):
            return False
        return True

    while waiting or state:
        # Units without a command finish as soon as what they wait for has.
        progressed = True
        while progressed:
            progressed = False
            for i in [i for i in waiting if not units[i].runs]:
                if ready(units[i]):
                    waiting.discard(i)
                    meta_at[i] = done_at[i] = t
                    progressed = True
        # Start what is ready, the longest remaining path first.
        used = [0] * len(machines)
        for m, _, _ in state.values():
            used[m] += 1
        cands = [i for i in waiting if units[i].runs and ready(units[i])]
        cands.sort(key=(lambda i: -level[i]) if order == "critical" else (lambda i: i))
        for i in cands:
            free = [m for m in range(len(machines)) if used[m] < machines[m]]
            if not free:
                break
            if place == "local":
                m = free[0]
            else:
                m = max(free, key=lambda m: machines[m] - used[m])
            used[m] += 1
            waiting.discard(i)
            state[i] = [m, 0, units[i].front]
        if not state:
            if waiting:
                sys.exit("the dependency graph does not resolve (a cycle?)")
            break
        # Cores nothing holds go to code generation on the same machine.
        extra = {i: 0 for i in state}
        for m in range(len(machines)):
            spare = machines[m] - used[m]
            gen = [i for i, s in state.items() if s[0] == m and s[1] == 1]
            while spare > 0 and gen:
                for i in list(gen):
                    if spare == 0:
                        break
                    if extra[i] + 1 < threads:
                        extra[i] += 1
                        spare -= 1
                    else:
                        gen.remove(i)
        t += dt
        for i in list(state):
            s = state[i]
            if s[1] == 0:
                s[2] -= dt
                if s[2] <= 0:
                    if units[i].pipelined:
                        meta_at[i] = t
                    s[1], s[2] = 1, units[i].work
            if s[1] == 1:
                s[2] -= dt * (1 + extra[i])
                if s[2] <= 0:
                    meta_at.setdefault(i, t)
                    done_at[i] = t
                    del state[i]
    return t


def main():
    sys.setrecursionlimit(100000)
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("report")
    ap.add_argument("shapes", nargs="*", default=["4", "8", "16", "24", "4+4", "8+8", "4+4+4"])
    ap.add_argument("--threads", type=int, default=16, help="code generation threads one compiler may use (default 16)")
    ap.add_argument("--json", action="store_true")
    a = ap.parse_args()
    data = load(a.report)
    units = {u["i"]: Unit(u, a.threads) for u in data["units"]}
    ran = [u for u in units.values() if u.runs]
    if any(u["cached"] for u in data["units"] if u["runs"] and u["end"] is not None):
        print("warning: some units came from the cache; their times are what producing them once cost", file=sys.stderr)
    rows = []
    for shape in a.shapes:
        machines = [int(x) for x in shape.split("+")]
        best = min(
            simulate(units, machines, a.threads, order, place)
            for order in ("critical", "plan")
            for place in ("local", "spread")
        )
        rows.append((shape, round(best, 1)))
    summary = dict(
        report=a.report,
        measured=dict(total_s=round(data["total"], 1), cores=data["cores"]),
        units=len(ran),
        cpu_s=round(sum(u.front + u.work for u in ran), 1),
        serial_s=round(sum(u.front for u in ran), 1),
        threads=a.threads,
        simulated=dict(rows),
    )
    if a.json:
        print(json.dumps(summary))
        return
    print(f"{a.report}: {len(ran)} units, {summary['cpu_s']} core-seconds ({summary['serial_s']} serial), "
          f"measured {summary['measured']['total_s']} s on {data['cores']} cores")
    for shape, secs in rows:
        print(f"  {shape:>8}  {secs:6.1f} s")


if __name__ == "__main__":
    main()
