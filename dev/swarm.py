#!/usr/bin/env python3
"""Simulated agent swarm: N agents in N git worktrees of one repository run
overlapping check / test / build commands with edits in between.

  swarm.py --repo ra --mode stock|rargo [--agents 20] [--keep]

Prints wall time, CPU time and disk use. `stock` gives every worktree its own
target directory, which is what a swarm of agents using plain cargo gets.
CPU time is read from the cgroup the work ran in: this script's own cgroup
for stock (run it under `systemd-run --scope`), rargod's for rargo.
"""
import argparse, os, subprocess, sys, time, shutil, threading, json

WORK = os.environ.get("RARGO_WORK", "/root/rargo-work")
RARGO = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "release", "rargo")

# Per repository: files agents edit (cheap leaf-ish and deep ones) and the
# packages they test.
REPOS = {
    "ra": dict(
        dir="ra",
        check=["check"],
        edits=["crates/stdx/src/lib.rs", "crates/parser/src/lib.rs", "crates/syntax/src/lib.rs", "crates/ide-db/src/lib.rs"],
        tests=[["test", "-p", "stdx"], ["test", "-p", "parser"], ["test", "-p", "syntax"]],
        build=["build", "-p", "rust-analyzer"],
    ),
    "ruff": dict(
        dir="ruff",
        check=["check"],
        edits=["crates/ruff_text_size/src/lib.rs", "crates/ruff_python_trivia/src/lib.rs", "crates/ruff_source_file/src/lib.rs", "crates/ruff_linter/src/lib.rs"],
        tests=[["test", "-p", "ruff_text_size"], ["test", "-p", "ruff_python_trivia"], ["test", "-p", "ruff_source_file"]],
        build=["build", "-p", "ruff"],
    ),
    "bevy": dict(
        dir="bevy",
        check=["check"],
        edits=["crates/bevy_utils/src/lib.rs", "crates/bevy_math/src/lib.rs", "crates/bevy_color/src/lib.rs", "crates/bevy_ecs/src/lib.rs"],
        tests=[["test", "-p", "bevy_utils"], ["test", "-p", "bevy_color"], ["test", "-p", "bevy_ptr"]],
        build=["build", "-p", "bevy_ecs"],
    ),
}

def cgroup_cpu_usec(pid):
    with open(f"/proc/{pid}/cgroup") as f:
        rel = f.read().strip().split("::", 1)[1]
    # rargod moves nothing; its scope is the parent of where it runs.
    path = "/sys/fs/cgroup" + rel
    while True:
        try:
            with open(path + "/cpu.stat") as f:
                for line in f:
                    if line.startswith("usage_usec"):
                        return int(line.split()[1])
        except FileNotFoundError:
            pass
        path = os.path.dirname(path)
        if path == "/sys/fs/cgroup":
            return 0

def du(path):
    out = subprocess.run(["du", "-sb", "--apparent-size", path], capture_output=True, text=True).stdout.split()
    real = subprocess.run(["du", "-sB1", path], capture_output=True, text=True).stdout.split()
    return (int(out[0]) if out else 0, int(real[0]) if real else 0)

def agent(i, wt, cfg, mode, env, log, results):
    def run(args):
        cmd = ([RARGO] if mode == "rargo" else ["cargo"]) + args
        if mode == "stock":
            cmd.append("--offline")
        t = time.time()
        p = subprocess.run(cmd, cwd=wt, env=env, stdout=log, stderr=subprocess.STDOUT)
        log.write(f"### agent {i} {' '.join(args)} -> {p.returncode} in {time.time()-t:.2f}s\n".encode())
        log.flush()
        return (" ".join(args), p.returncode, time.time() - t)

    def edit(n):
        # Agents overlap: several make the same edit to the same file.
        f = cfg["edits"][(i + n) % len(cfg["edits"])]
        variant = (i // len(cfg["edits"]) + n) % 3
        with open(os.path.join(wt, f), "a") as fh:
            fh.write(f"\n#[allow(dead_code)]\npub fn rargo_bench_{n}_{variant}() -> u32 {{ {variant} + {n} }}\n")

    steps = []
    steps.append(run(cfg["check"]))
    edit(0)
    steps.append(run(cfg["check"]))
    steps.append(run(cfg["tests"][i % len(cfg["tests"])]))
    edit(1)
    steps.append(run(cfg["check"]))
    steps.append(run(cfg["tests"][(i + 1) % len(cfg["tests"])]))
    steps.append(run(cfg["build"]))
    steps.append(run(cfg["tests"][i % len(cfg["tests"])]))
    results[i] = steps

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", required=True, choices=REPOS)
    ap.add_argument("--mode", required=True, choices=["stock", "rargo"])
    ap.add_argument("--agents", type=int, default=20)
    ap.add_argument("--keep", action="store_true")
    ap.add_argument("--out", default=None)
    a = ap.parse_args()
    cfg = REPOS[a.repo]
    base = os.path.join(WORK, "bench", cfg["dir"])
    root = os.path.join(WORK, "swarm", f"{a.repo}-{a.mode}")
    subprocess.run(["git", "-C", base, "worktree", "prune"], check=True)
    if os.path.exists(root):
        for d in os.listdir(root):
            subprocess.run(["git", "-C", base, "worktree", "remove", "--force", os.path.join(root, d)], capture_output=True)
        shutil.rmtree(root, ignore_errors=True)
    os.makedirs(root)
    wts = []
    for i in range(a.agents):
        wt = os.path.join(root, f"agent{i:02d}")
        subprocess.run(["git", "-C", base, "worktree", "add", "-q", "--detach", wt], check=True)
        wts.append(wt)

    env = dict(os.environ)
    daemon_pid = None
    if a.mode == "rargo":
        sock = env.get("RARGO_SOCKET", os.path.join(WORK, "state", "rargod.sock"))
        env["RARGO_SOCKET"] = sock
        out = subprocess.run(["pgrep", "-x", "rargod"], capture_output=True, text=True).stdout.split()
        daemon_pid = int(out[0]) if out else None
        cpu0 = cgroup_cpu_usec(daemon_pid)
    else:
        cpu0 = cgroup_cpu_usec(os.getpid())

    results = {}
    logs = [open(os.path.join(root, f"agent{i:02d}.log"), "wb") for i in range(a.agents)]
    t0 = time.time()
    threads = [threading.Thread(target=agent, args=(i, wts[i], cfg, a.mode, env, logs[i], results)) for i in range(a.agents)]
    for t in threads: t.start()
    for t in threads: t.join()
    wall = time.time() - t0
    cpu1 = cgroup_cpu_usec(daemon_pid) if a.mode == "rargo" else cgroup_cpu_usec(os.getpid())

    failures = [(i, s) for i, steps in results.items() for s in steps if s[1] != 0]
    apparent = real = 0
    for wt in wts:
        ap_, re_ = du(os.path.join(wt, "target"))
        apparent += ap_; real += re_
    store = (0, 0)
    store_after_gc = (0, 0)
    if a.mode == "rargo":
        store = du(os.path.join(os.path.dirname(env["RARGO_SOCKET"]), "store"))
        subprocess.run([RARGO, "--rargo-gc"], env=env, capture_output=True)
        store_after_gc = du(os.path.join(os.path.dirname(env["RARGO_SOCKET"]), "store"))
    per_step = {}
    for steps in results.values():
        for n, (name, rc, dt) in enumerate(steps):
            per_step.setdefault(n, []).append(dt)
    summary = dict(
        repo=a.repo, mode=a.mode, agents=a.agents,
        wall_s=round(wall, 1), cpu_s=round((cpu1 - cpu0) / 1e6, 1),
        worktree_target_bytes=real, worktree_target_apparent_bytes=apparent,
        store_bytes=store[1],
        store_after_gc_bytes=store_after_gc[1],
        disk_total_bytes=(real if a.mode == "stock" else store[1]),
        failures=len(failures),
        step_mean_s=[round(sum(v) / len(v), 2) for _, v in sorted(per_step.items())],
        step_max_s=[round(max(v), 2) for _, v in sorted(per_step.items())],
    )
    print(json.dumps(summary))
    if failures:
        print("failed steps:", failures[:10], file=sys.stderr)
    if a.out:
        with open(a.out, "a") as f:
            f.write(json.dumps(summary) + "\n")
    if not a.keep:
        for wt in wts:
            subprocess.run(["git", "-C", base, "worktree", "remove", "--force", wt], capture_output=True)
        shutil.rmtree(root, ignore_errors=True)

if __name__ == "__main__":
    main()
