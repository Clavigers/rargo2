#!/usr/bin/env python3
"""Replays a repository's recent history through rargo, one commit at a time.

For each of the last N first-parent commits (oldest first) the worktree is
moved to the commit and one cargo command is run. Reported per commit: wall
time, actions run and their CPU time. The first commit is the cold build;
the rest show what a real change costs to rebuild.

Used to compare compilers under rargo (stock rustc against rargo's fork,
whose crate hash survives edits that do not change what dependents can
see): same daemon options, same history, different RUSTUP_TOOLCHAIN.

  dev/replay.py --repo ra --commits 30 --toolchain rargo -- build -p rust-analyzer
"""
import argparse, json, os, re, subprocess, sys, time

WORK = os.environ.get("RARGO_WORK", "/root/rargo-work")
# Set by --netns: the command that puts the client into its network namespace.
PREFIX = []


def status(rargo, env):
    out = subprocess.run([*PREFIX, rargo, "--rargo-status"], env=env, capture_output=True, text=True).stdout
    m = re.search(r"actions run\s+(\d+) \(([\d.]+)s cpu", out)
    c = re.search(r"cache\s+(\d+) hits, (\d+) misses", out)
    return int(m.group(1)), float(m.group(2)), int(c.group(1))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", required=True)
    ap.add_argument("--commits", type=int, default=30)
    ap.add_argument("--toolchain", default=None)
    ap.add_argument("--rargo", default=os.path.join(WORK, "dev-target/release/rargo"))
    ap.add_argument("--label", default=None)
    ap.add_argument("--out", default=None)
    ap.add_argument("--netns", default=None, help="network namespace the rargo client runs in (dev/netns.sh)")
    ap.add_argument("--stock", action="store_true", help="stock cargo (24 cores) in the worktree instead of rargo")
    ap.add_argument("cmd", nargs="+")
    a = ap.parse_args()
    if a.netns:
        PREFIX.extend(["nsenter", f"--net=/run/netns/{a.netns}"])

    repo = os.path.join(WORK, "bench", a.repo)
    wt = os.path.join(WORK, "bench", f"replay-{a.repo}")
    git = lambda *args, cwd=repo: subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, text=True).stdout
    commits = git("log", "--first-parent", "--format=%H", f"-{a.commits}").split()[::-1]
    if os.path.exists(wt):
        git("worktree", "remove", "--force", wt)
    git("worktree", "add", "--detach", wt, commits[0])

    env = dict(os.environ)
    if a.toolchain:
        env["RUSTUP_TOOLCHAIN"] = a.toolchain
    rows = []
    for n, c in enumerate(commits):
        git("checkout", "-q", "--detach", c, cwd=wt)
        files = 0
        if n:
            files = len(git("diff", "--name-only", commits[n - 1], c, "--", "*.rs", cwd=wt).split())
        if a.stock:
            a0 = cpu0 = hit0 = a1 = cpu1 = hit1 = 0
            t = time.time()
            p = subprocess.run(["systemd-run", "--scope", "-q", "-p", "CPUQuota=2400%", "-p", "MemoryMax=90G",
                                "cargo", *a.cmd, "--offline", "-j24"], cwd=wt, env=env, capture_output=True, text=True)
            dt = time.time() - t
        else:
            a0, cpu0, hit0 = status(a.rargo, env)
            t = time.time()
            p = subprocess.run([*PREFIX, a.rargo, *a.cmd], cwd=wt, env=env, capture_output=True, text=True)
            dt = time.time() - t
            a1, cpu1, hit1 = status(a.rargo, env)
        row = dict(commit=c[:9], rs_files=files, wall_s=round(dt, 2), actions=a1 - a0, cpu_s=round(cpu1 - cpu0, 1),
                   hits=hit1 - hit0, ok=p.returncode == 0)
        rows.append(row)
        print(json.dumps(row), flush=True)
        if p.returncode != 0:
            sys.stderr.write(p.stderr[-2000:])
    later = rows[1:]
    walls = sorted(r["wall_s"] for r in later)
    pick = lambda q: walls[min(len(walls) - 1, int(q * len(walls)))] if walls else 0
    summary = dict(
        label=a.label or a.toolchain or "default",
        repo=a.repo,
        cmd=a.cmd,
        commits=len(rows),
        cold=rows[0],
        later_wall_s=round(sum(r["wall_s"] for r in later), 1),
        later_median_s=pick(0.5), later_p90_s=pick(0.9), later_max_s=walls[-1] if walls else 0,
        later_actions=sum(r["actions"] for r in later),
        later_cpu_s=round(sum(r["cpu_s"] for r in later), 1),
        failed=sum(1 for r in rows if not r["ok"]),
        rows=rows,
    )
    print(json.dumps({k: v for k, v in summary.items() if k != "rows"}))
    if a.out:
        with open(a.out, "a") as f:
            f.write(json.dumps(summary) + "\n")
    # The client daemon's output directory is unmounted before the
    # worktree goes.
    if not a.stock:
        subprocess.run([*PREFIX, a.rargo, "--rargo-daemon-stop"], env=env, capture_output=True)
        time.sleep(1)
    git("worktree", "remove", "--force", wt)


if __name__ == "__main__":
    main()
