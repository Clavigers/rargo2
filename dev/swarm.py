#!/usr/bin/env python3
"""Simulated agent swarm: N agents in N git worktrees of one repository run
overlapping check / test / build commands with edits in between.

  swarm.py --repo ra --mode stock|rargo [--agents 20] [--keep]

Prints wall time, CPU time and disk use. `stock` gives every worktree its own
target directory, which is what a swarm of agents using plain cargo gets.
CPU time is read from the cgroup the work ran in: this script's own cgroup
for stock (run it under `systemd-run --scope`), the server's for rargo (the
clients' own CPU time is reported separately).

In rargo mode the agents are clients of the build server at RARGO_SERVER
whose state directory is RARGO_STATE; with --netns they run in that network
namespace (see dev/netns.sh), so every byte crosses the shaped link.
"""
import argparse, os, subprocess, sys, time, shutil, threading, json

WORK = os.environ.get("RARGO_WORK", "/root/rargo-work")
RARGO = os.path.join(os.environ.get("RARGO_BIN") or os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "release"), "rargo")

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
    # Zed needs the dev shell of its flake for system libraries: rargo
    # evaluates it on the server, stock cargo runs inside it (zed-cargo.sh).
    "zed": dict(
        dir="zed",
        check=["check"],
        edits=["crates/collections/src/collections.rs", "crates/sum_tree/src/sum_tree.rs", "crates/rope/src/rope.rs", "crates/text/src/text.rs"],
        tests=[["test", "-p", "collections"], ["test", "-p", "sum_tree"], ["test", "-p", "rope"]],
        build=["build"],
        cargo=os.path.join(os.path.dirname(os.path.abspath(__file__)), "zed-cargo.sh"),
    ),
}

def cgroup_cpu_usec(pid):
    with open(f"/proc/{pid}/cgroup") as f:
        rel = f.read().strip().split("::", 1)[1]
    # rargod puts itself in a `daemon` child of its scope and each action in
    # a sibling; the scope is what everything ran in.
    if rel.endswith("/daemon"):
        rel = os.path.dirname(rel)
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

def agent(i, wt, cfg, mode, env, log, results, edit_kind="api", prefix=()):
    def run(args):
        cmd = (list(prefix) + [RARGO] if mode == "rargo" else [cfg.get("cargo", "cargo")]) + args
        if mode == "stock":
            cmd.append("--offline")
        t = time.time()
        log.flush()
        at = os.path.getsize(log.name)
        p = subprocess.run(cmd, cwd=wt, env=env, stdout=log, stderr=subprocess.STDOUT)
        log.write(f"### agent {i} {' '.join(args)} -> {p.returncode} in {time.time()-t:.2f}s\n".encode())
        log.flush()
        out = ""
        if p.returncode != 0:
            # What the failed command itself printed.
            with open(log.name, "rb") as f:
                f.seek(at)
                out = f.read().decode(errors="replace")[-3000:]
        return (" ".join(args), p.returncode, time.time() - t, out)

    def edit(n):
        # Agents overlap: several make the same edit to the same file.
        # A body-only second edit changes the function the first one added.
        f = cfg["edits"][(i + (0 if edit_kind == "body" else n)) % len(cfg["edits"])]
        variant = (i // len(cfg["edits"]) + n) % 3
        path = os.path.join(wt, f)
        if edit_kind == "body" and n > 0:
            # Change only the inside of the function the first edit added:
            # nothing other crates can see moves.
            text = open(path).read()
            marker = "    let rargo_bench_value = "
            at = text.rindex(marker)
            end = text.index("\n", at)
            text = text[:at] + f"{marker}{variant} * 7 + {n};" + text[end:]
            open(path, "w").write(text)
            return
        with open(path, "a") as fh:
            if edit_kind == "body":
                fh.write(f"\n#[allow(dead_code)]\npub fn rargo_bench_{variant}() -> u32 {{\n    let rargo_bench_value = {variant} + {n};\n    rargo_bench_value\n}}\n")
            else:
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
    ap.add_argument("--label", default=None, help="recorded with the result")
    ap.add_argument("--toolchain", default=None, help="RUSTUP_TOOLCHAIN for the agents")
    ap.add_argument("--netns", default=None, help="network namespace the rargo clients run in")
    ap.add_argument("--batch", type=int, default=0,
                    help="run the agents in consecutive groups of this size (all worktrees and target directories are kept "
                         "until the end); for a stock swarm that does not fit in memory at once")
    ap.add_argument("--edit", default="api", choices=["api", "body"],
                    help="api: both edits add a public function; body: the second edit changes only a function body")
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
    if a.toolchain:
        env["RUSTUP_TOOLCHAIN"] = a.toolchain
    daemon_pid = None
    # Only the network namespace is entered (`ip netns exec` would also give
    # every command a mount namespace of its own, and the client daemon's
    # virtual output directories would be invisible to the next command).
    prefix = ["nsenter", f"--net=/run/netns/{a.netns}"] if a.netns else []
    state = env.get("RARGO_STATE", os.path.join(WORK, "state"))
    client_cache = os.path.join(root, "client-cache")
    if a.mode == "rargo":
        env["RARGO_CACHE_DIR"] = client_cache
        # The server with this state directory (others may be running).
        for pid in subprocess.run(["pgrep", "-x", "rargod"], capture_output=True, text=True).stdout.split():
            try:
                cmdline = open(f"/proc/{pid}/cmdline").read().split("\0")
            except OSError:
                continue
            if "--state" in cmdline and cmdline[cmdline.index("--state") + 1] == state:
                daemon_pid = int(pid)
        if daemon_pid is None:
            sys.exit(f"no rargod found for {state}")
        cpu0 = cgroup_cpu_usec(daemon_pid)
    else:
        cpu0 = cgroup_cpu_usec(os.getpid())
    self0 = os.times()

    results = {}
    logs = [open(os.path.join(root, f"agent{i:02d}.log"), "wb") for i in range(a.agents)]
    # Bytes crossing the client link, both ways, as the client's interface
    # counts them.
    def link_bytes():
        if not a.netns:
            return (0, 0)
        read = lambda f: int(subprocess.run(["ip", "netns", "exec", a.netns, "cat", f"/sys/class/net/eth0/statistics/{f}"],
                                            capture_output=True, text=True).stdout.strip() or 0)
        return (read("rx_bytes"), read("tx_bytes"))
    link0 = link_bytes()
    t0 = time.time()
    threads = [threading.Thread(target=agent, args=(i, wts[i], cfg, a.mode, env, logs[i], results, a.edit, prefix)) for i in range(a.agents)]
    size = a.batch if a.batch > 0 else a.agents
    for first in range(0, a.agents, size):
        group = threads[first:first + size]
        for t in group: t.start()
        for t in group: t.join()
    wall = time.time() - t0
    link1 = link_bytes()
    daemon_status = ""
    daemon_cpu = 0.0
    if a.mode == "rargo":
        # What the client daemon did, and then its end: its virtual output
        # directories are unmounted, so that what is measured below is what
        # is really on the client's disk.
        daemon_status = subprocess.run(prefix + [RARGO, "--rargo-daemon-status"], env=env, capture_output=True, text=True).stdout
        # The daemon's own CPU time (it is not a child of the commands).
        tick = os.sysconf("SC_CLK_TCK")
        for pid in subprocess.run(["pgrep", "-f", f"--rargo-daemon {client_cache}"], capture_output=True, text=True).stdout.split():
            try:
                fields = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
                daemon_cpu += (int(fields[11]) + int(fields[12])) / tick
            except (OSError, IndexError, ValueError):
                pass
        subprocess.run(prefix + [RARGO, "--rargo-daemon-stop"], env=env, capture_output=True)
        time.sleep(1)
    cpu1 = cgroup_cpu_usec(daemon_pid) if a.mode == "rargo" else cgroup_cpu_usec(os.getpid())

    self1 = os.times()
    client_cpu = (self1.children_user + self1.children_system) - (self0.children_user + self0.children_system)
    failures = [(i, s) for i, steps in results.items() for s in steps if s[1] != 0]
    targets = [os.path.join(wt, "target") for wt in wts]
    # One `du` over everything on the client side, so that an artifact
    # shared between worktrees through the client's cache counts once.
    def du_all(paths):
        paths = [p for p in paths if os.path.exists(p)]
        if not paths:
            return 0
        out = subprocess.run(["du", "-scB1"] + paths, capture_output=True, text=True).stdout.strip().splitlines()
        return int(out[-1].split()[0]) if out else 0
    real = du_all(targets + [client_cache])
    apparent = sum(du(t)[0] for t in targets)
    store = (0, 0)
    store_after_gc = (0, 0)
    sync_line = ""
    if a.mode == "rargo":
        store_dir = env.get("RARGO_STORE", os.path.join(state, "store"))
        store = du(store_dir)
        status = subprocess.run(prefix + [RARGO, "--rargo-status"], env=env, capture_output=True, text=True).stdout
        sync_line = next((l for l in status.splitlines() if l.startswith("sync")), "")
        subprocess.run(prefix + [RARGO, "--rargo-gc"], env=env, capture_output=True)
        store_after_gc = du(store_dir)
    per_step = {}
    for steps in results.values():
        for n, (name, rc, dt, _) in enumerate(steps):
            per_step.setdefault(n, []).append(dt)
    summary = dict(
        repo=a.repo, mode=a.mode, agents=a.agents, label=a.label, toolchain=a.toolchain, edit=a.edit,
        wall_s=round(wall, 1), cpu_s=round((cpu1 - cpu0) / 1e6, 1),
        client_cpu_s=round(client_cpu, 1) if a.mode == "rargo" else 0,
        client_bytes=real, worktree_target_apparent_bytes=apparent,
        store_bytes=store[1],
        store_after_gc_bytes=store_after_gc[1],
        disk_total_bytes=real + store[1],
        netns=a.netns, sync=sync_line,
        link_down_bytes=link1[0] - link0[0], link_up_bytes=link1[1] - link0[1],
        output_mode=env.get("RARGO_OUTPUT", "fuse") if a.mode == "rargo" else None,
        client_daemon_cpu_s=round(daemon_cpu, 1),
        client_daemon=" | ".join(l.strip() for l in daemon_status.splitlines() if l.startswith(("link", "scans", "pushed", "outputs"))),
        failures=len(failures),
        # Which step of which agent, and the end of that agent's log.
        failed_steps=[dict(agent=i, step=st[0], rc=st[1], output=st[3]) for i, st in failures[:5]],
        step_mean_s=[round(sum(v) / len(v), 2) for _, v in sorted(per_step.items())],
        step_max_s=[round(max(v), 2) for _, v in sorted(per_step.items())],
    )
    print(json.dumps(summary))
    if failures:
        print("failed steps:", [f[1][:3] for f in failures[:10]], file=sys.stderr)
    if a.out:
        with open(a.out, "a") as f:
            f.write(json.dumps(summary) + "\n")
    if not a.keep:
        for wt in wts:
            subprocess.run(["git", "-C", base, "worktree", "remove", "--force", wt], capture_output=True)
        shutil.rmtree(root, ignore_errors=True)

if __name__ == "__main__":
    main()
