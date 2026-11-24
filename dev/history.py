#!/usr/bin/env python3
"""The history benchmark: walk a repository's first-parent history and
build at its commits, four ways.

  a  stock cargo, one worktree with a persistent target directory,
     building every commit
  b  stock cargo, cold (empty target directory) at every commit
  c  stock cargo, one worktree with a persistent target directory,
     building only every 10th commit (the commits in between are holes
     for it)
  d  rargo, two agents interleaved in commit order: one builds every
     commit that is not a 10th commit, the other only every 10th

  dev/history.py fetch --commits FILE
  dev/history.py a|b|c|d --commits FILE --out FILE.jsonl [--cores N]
                 [--lane K]                 (b: one of several workers, each with its own worktree)
                 [--netns NS --rargo PATH]  (d: the rig of dev/netns.sh)
  dev/history.py stats --out FILE.jsonl

FILE has one line per commit, oldest first: `<commit> <toolchain>` (each
commit is built with the toolchain its workspace names as its minimum; a
two-year-old commit does not compile with today's). Every build is one
JSON line in the output; a run that is started again continues where it
stopped. Builds are `build -p rust-analyzer`, capped at N cores.
"""
import argparse, json, os, subprocess, sys, time

WORK = os.environ.get("RARGO_WORK", "/root/rargo-work")
REPO = os.path.join(WORK, "bench", "ra-hist")
ROOT = os.path.join(WORK, "hist")
TARGET = ["build", "-p", "rust-analyzer"]


def git(*args, cwd=REPO, check=True):
    return subprocess.run(["git", *args], cwd=cwd, check=check, capture_output=True, text=True).stdout


def checkout(wt, commit):
    """Moves a worktree to a commit; files earlier commits had and this one
    does not go, the output directory stays."""
    git("checkout", "-q", "-f", "--detach", commit, cwd=wt)
    git("clean", "-fdq", "-e", "target", cwd=wt, check=False)


def worktree(name, commit):
    wt = os.path.join(ROOT, name)
    if not os.path.isdir(wt):
        os.makedirs(ROOT, exist_ok=True)
        # (No `git worktree prune` here: several walks start at once, and
        # one's prune took away the worktree another was just adding.)
        git("worktree", "add", "--detach", wt, commit)
    return wt


def done(out, variant):
    seen = set()
    if os.path.exists(out):
        for line in open(out):
            try:
                d = json.loads(line)
            except ValueError:
                continue
            if d.get("variant") == variant:
                seen.add(d["i"])
    return seen


def record(out, row):
    with open(out, "a") as f:
        f.write(json.dumps(row) + "\n")


def stock(wt, toolchain, cores, online=False):
    cmd = ["systemd-run", "--scope", "-q", "-p", f"CPUQuota={cores * 100}%", "-p", "MemoryMax=30G",
           "cargo", f"+{toolchain}", *TARGET, f"-j{cores}"] + ([] if online else ["--offline"])
    t = time.time()
    p = subprocess.run(cmd, cwd=wt, capture_output=True, text=True)
    return time.time() - t, p


def tail(p):
    return (p.stderr or "")[-400:] if p.returncode != 0 else ""


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("variant", choices=["fetch", "a", "b", "c", "d", "stats"])
    ap.add_argument("--commits")
    ap.add_argument("--out")
    ap.add_argument("--cores", type=int, default=6)
    ap.add_argument("--lane", type=int, default=0)
    ap.add_argument("--netns", default=None)
    ap.add_argument("--cargo", default=None, help="fetch: the cargo binary to fetch with")
    ap.add_argument("--rargo", default=os.path.join(WORK, "bin-hist", "rargo"))
    a = ap.parse_args()
    if a.variant == "stats":
        return stats(a.out)
    commits = [l.split() for l in open(a.commits) if l.strip()]

    if a.variant == "fetch":
        # Every crate any commit's lock file names, into the cargo home.
        wt = worktree("fetch", commits[0][0])
        prev = None
        for i, (c, tc) in enumerate(commits, 1):
            if prev and not git("diff", "--name-only", prev, c, "--", "Cargo.lock").strip() and commits[i - 2][1] == tc:
                prev = c
                continue
            checkout(wt, c)
            # With the toolchain's own cargo, or with the one given (the
            # server's planner keeps its registry cache under another
            # directory name than older cargos do).
            cargo = [a.cargo] if a.cargo else ["cargo", f"+{tc}"]
            env = dict(os.environ, RUSTUP_TOOLCHAIN=tc)
            p = subprocess.run([*cargo, "fetch", "--locked"], cwd=wt, env=env, capture_output=True, text=True)
            if p.returncode != 0:
                p = subprocess.run([*cargo, "fetch"], cwd=wt, env=env, capture_output=True, text=True)
            if p.returncode != 0:
                print(i, c[:9], "fetch failed:", p.stderr[-200:], flush=True)
            prev = c
        return

    seen = done(a.out, a.variant)
    if a.variant in ("a", "c"):
        wt = worktree(a.variant, commits[0][0])
        for i, (c, tc) in enumerate(commits, 1):
            if (a.variant == "c" and i % 10 != 0) or i in seen:
                continue
            checkout(wt, c)
            dt, p = stock(wt, tc, a.cores)
            record(a.out, dict(variant=a.variant, i=i, commit=c[:10], toolchain=tc, wall_s=round(dt, 2), rc=p.returncode, err=tail(p)))
    elif a.variant == "b":
        # Any number of these run side by side (cold builds do not depend
        # on each other): each takes the next commit nobody has claimed.
        wt = worktree(f"b{a.lane}", commits[0][0])
        claims = os.path.join(ROOT, "claims")
        os.makedirs(claims, exist_ok=True)
        for i, (c, tc) in enumerate(commits, 1):
            if i in seen:
                continue
            try:
                os.close(os.open(os.path.join(claims, f"b-{i}"), os.O_CREAT | os.O_EXCL | os.O_WRONLY))
            except FileExistsError:
                continue
            checkout(wt, c)
            subprocess.run(["rm", "-rf", os.path.join(wt, "target")])
            dt, p = stock(wt, tc, a.cores)
            record(a.out, dict(variant="b", i=i, commit=c[:10], toolchain=tc, wall_s=round(dt, 2), rc=p.returncode, err=tail(p)))
    elif a.variant == "d":
        prefix = ["nsenter", f"--net=/run/netns/{a.netns}"] if a.netns else []
        wts = {"every": worktree("d-every", commits[0][0]), "tenth": worktree("d-tenth", commits[0][0])}
        for i, (c, tc) in enumerate(commits, 1):
            if i in seen:
                continue
            agent = "tenth" if i % 10 == 0 else "every"
            wt = wts[agent]
            checkout(wt, c)
            env = dict(os.environ, RUSTUP_TOOLCHAIN=tc)
            t = time.time()
            p = subprocess.run([*prefix, a.rargo, *TARGET], cwd=wt, env=env, capture_output=True, text=True)
            dt = time.time() - t
            record(a.out, dict(variant="d", i=i, agent=agent, commit=c[:10], toolchain=tc, wall_s=round(dt, 2), rc=p.returncode, err=tail(p)))


def summary(walls):
    walls = sorted(walls)
    if not walls:
        return dict(n=0)
    pick = lambda q: walls[min(len(walls) - 1, int(q * len(walls)))]
    return dict(n=len(walls), median=pick(0.5), p90=pick(0.9), max=walls[-1], total=round(sum(walls), 1))


def stats(out):
    rows = {}
    for line in open(out):
        try:
            d = json.loads(line)
        except ValueError:
            continue
        key = d["variant"] if d["variant"] != "d" else "d-" + d["agent"]
        rows.setdefault(key, {})[d["i"]] = d
    # Builds that were in progress while the benchmark was stopped for a
    # while (their times include the pause) are left out.
    tainted = {}
    path = os.path.join(os.path.dirname(out), "history-tainted.json")
    if os.path.exists(path):
        tainted = {k: set(v) for k, v in json.load(open(path)).items()}
    for k in list(rows):
        skip = tainted.get(k[0], set())
        left = sum(1 for i in rows[k] if i in skip)
        rows[k] = {i: d for i, d in rows[k].items() if i not in skip}
        if left:
            print(k, "left out (in progress during a pause):", left)
    ok = lambda k: {i: d["wall_s"] for i, d in rows.get(k, {}).items() if d["rc"] == 0}
    for k in sorted(rows):
        failed = sum(1 for d in rows[k].values() if d["rc"] != 0)
        print(k, json.dumps(dict(summary(ok(k).values()), built=len(rows[k]), failed=failed)))
    # The holes: the 10th commits, where both stock cargo (c) and rargo's
    # second agent (d) succeeded.
    c, t = ok("c"), ok("d-tenth")
    both = sorted(set(c) & set(t))
    print("holes, c on common commits", json.dumps(summary([c[i] for i in both])))
    print("holes, d on common commits", json.dumps(summary([t[i] for i in both])))
    a, e = ok("a"), ok("d-every")
    both = sorted(set(a) & set(e))
    print("every commit but the 10th, a on common commits", json.dumps(summary([a[i] for i in both])))
    print("every commit but the 10th, d on common commits", json.dumps(summary([e[i] for i in both])))


if __name__ == "__main__":
    main()
