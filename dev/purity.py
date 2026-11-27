import os
import re
import resource
import subprocess
import sys

BUDGETS = "crates/rargo-purity/budgets.txt"
ALLOW = "crates/rargo-purity/allow.txt"
SCANNER = "crates/rargo-purity/src/"
PURITY = "crates/rargo-purity/"
MEASURE = "cpu-across-runners"


def rows(text):
    return [line.split("\t") for line in text.splitlines() if line.strip()]


def measure(text):
    for cols in rows(text):
        if cols[0] == "measure":
            return cols[1]
    return None


def parse_budgets(text):
    suite, default, slow = None, None, {}
    for cols in rows(text):
        if len(cols) != 4 or not cols[2].isdigit() or len(cols[3].strip()) < 10:
            sys.exit(f"{BUDGETS}: want kind, name, milliseconds and a reason separated by tabs: {cols}")
        kind, name, ms = cols[0], cols[1], int(cols[2])
        if kind == "measure":
            continue
        if kind == "suite":
            suite = ms
        elif kind == "test":
            default = ms
        elif kind == "slow":
            if name in slow:
                sys.exit(f"{BUDGETS}: {name} has two lines")
            slow[name] = ms
        else:
            sys.exit(f"{BUDGETS}: no kind named {kind}")
    if suite is None or default is None:
        sys.exit(f"{BUDGETS}: wants a suite line and a test line")
    return suite, default, slow


def parse_allow(text):
    return {(c[0], c[1]): int(c[2]) for c in rows(text)}


def programs(extra):
    built = subprocess.run(
        ["rargo", "test", "--locked", "--workspace", "--no-run", *extra], capture_output=True, text=True
    )
    if built.returncode != 0:
        sys.stdout.write(built.stdout + built.stderr)
        sys.exit("the test programs did not build")
    helper = subprocess.run(["rargo", "build", "--locked", *extra, "-p", "rargo-core"], capture_output=True, text=True)
    if helper.returncode != 0:
        sys.stdout.write(helper.stdout + helper.stderr)
        sys.exit("rargo-core's binaries did not build")
    found = re.findall(r"\((target/[^)]*)\)", built.stderr)
    if not found:
        sys.exit("no test programs in the build output:\n" + built.stderr)
    return found


def cpu_of(argv, cwd, env):
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    run = subprocess.run(argv, cwd=cwd, env=env, capture_output=True, text=True)
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    used = (after.ru_utime - before.ru_utime) + (after.ru_stime - before.ru_stime)
    return used * 1000, run


def cpu(extra):
    if measure(open(BUDGETS).read()) != MEASURE:
        sys.exit(f"{BUDGETS} must measure {MEASURE}, which is what this script measures")
    suite, default, slow = parse_budgets(open(BUDGETS).read())
    root = os.getcwd()
    asroot = [] if os.geteuid() == 0 else ["sudo", "-E"]
    took, failed = {}, []
    for program in programs(extra):
        package = program.split("/")[3]
        crate = os.path.join(root, "crates", package)
        name = re.sub(r"-[0-9a-f]{16}$", "", os.path.basename(program))
        env = dict(os.environ, CARGO_MANIFEST_DIR=crate, CARGO_PKG_NAME=package)
        listed = subprocess.run([os.path.join(root, program), "--list", "--format", "terse"],
                                cwd=crate, env=env, capture_output=True, text=True)
        tests = [line[: -len(": test")] for line in listed.stdout.splitlines() if line.endswith(": test")]
        for test in tests:
            key = f"{name} {test}"
            ms, run = cpu_of([*asroot, os.path.join(root, program), "--exact", test, "--test-threads=1", "-q"],
                             crate, env)
            took[key] = ms
            if run.returncode != 0:
                failed.append(key)
                sys.stdout.write(f"---- {key} ----\n{run.stdout}{run.stderr}\n")
    problems = [f"{key} failed" for key in failed]
    total = sum(took.values())
    over = []
    if total > suite:
        over.append(f"the suite used {total:.0f} ms of CPU; its budget is {suite} ms")
    for key, ms in sorted(took.items()):
        limit = slow.get(key, default)
        if ms > limit:
            over.append(f"{key} used {ms:.0f} ms of CPU; its budget is {limit} ms")
    problems += over
    for key, limit in sorted(slow.items()):
        if key not in took:
            problems.append(f"{BUDGETS} gives {key} a budget, but no test of that name ran; remove the line")
        elif took[key] <= default:
            print(f"note: {key} used {took[key]:.0f} ms of CPU here, within the default {default} ms; its line may go once it does so on every machine")
    if not took:
        problems.append("no tests ran")
    slowest = max(took.items(), key=lambda kv: kv[1], default=("none", 0))
    print(f"{len(took)} tests used {total:.0f} ms of CPU of a {suite} ms budget; the most was {slowest[1]:.0f} ms, by {slowest[0]}")
    for key, ms in sorted(took.items(), key=lambda kv: -kv[1])[:8]:
        print(f"  {ms:7.0f} ms  {key}")
    if os.environ.get("RARGO_CPU_REPORT"):
        with open(os.environ["RARGO_CPU_REPORT"], "w") as out:
            for key, ms in sorted(took.items(), key=lambda kv: -kv[1]):
                out.write(f"{ms:.1f}\t{key}\n")
    for p in problems:
        print(f"FAIL {p}")
    sys.exit(1 if problems else 0)


def at(base, path):
    shown = subprocess.run(["git", "show", f"{base}:{path}"], capture_output=True, text=True)
    return shown.stdout if shown.returncode == 0 else None


def ratchet(base):
    problems = []
    changed = subprocess.run(["git", "diff", "--name-only", base], capture_output=True, text=True, check=True).stdout.split()
    scanner = any(f.startswith(SCANNER) for f in changed) and all(f.startswith(PURITY) for f in changed)
    old = at(base, ALLOW)
    if old is not None:
        before, now = parse_allow(old), parse_allow(open(ALLOW).read())
        grown = []
        for key, count in sorted(now.items()):
            if key not in before:
                grown.append(f"{ALLOW} gains an entry: {key[0]} {key[1]}")
            elif count > before[key]:
                grown.append(f"{ALLOW} raises {key[0]} {key[1]} from {before[key]} to {count}")
        if scanner and grown:
            print(f"this change touches only {PURITY}, so the scanner's new findings are recorded, not added:")
            for g in grown:
                print(f"  {g}")
        elif grown and any(f.startswith(SCANNER) for f in changed):
            problems.append(f"a change to the scanner may record new entries only if it changes nothing outside {PURITY}")
            problems.extend(grown)
        else:
            problems.extend(grown)
    old = at(base, BUDGETS)
    if measure(open(BUDGETS).read()) != MEASURE:
        problems.append(f"{BUDGETS} must measure {MEASURE}")
    if old is not None and measure(old) != measure(open(BUDGETS).read()):
        problems.append(f"{BUDGETS} changes its measure from {measure(old)} to {measure(open(BUDGETS).read())}")
    if old is not None:
        (s0, d0, slow0), (s1, d1, slow1) = parse_budgets(old), parse_budgets(open(BUDGETS).read())
        if s1 > s0:
            problems.append(f"{BUDGETS} raises the suite budget from {s0} to {s1} ms")
        if d1 > d0:
            problems.append(f"{BUDGETS} raises the default test budget from {d0} to {d1} ms")
        for name, ms in sorted(slow1.items()):
            if name not in slow0:
                problems.append(f"{BUDGETS} gains a slow test: {name}")
            elif ms > slow0[name]:
                problems.append(f"{BUDGETS} raises {name} from {slow0[name]} to {ms} ms")
    for p in problems:
        print(f"FAIL {p}")
    print("the allowlist and the budgets only shrink" if not problems else f"{len(problems)} increases against {base}")
    sys.exit(1 if problems else 0)


if __name__ == "__main__":
    if sys.argv[1:2] == ["cpu"]:
        cpu(sys.argv[2:])
    elif len(sys.argv) == 3 and sys.argv[1] == "ratchet":
        ratchet(sys.argv[2])
    else:
        sys.exit("usage: purity.py cpu [rargo test options] | purity.py ratchet BASE")
