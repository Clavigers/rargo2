import json
import subprocess
import sys

BUDGETS = "crates/rargo-purity/budgets.txt"
ALLOW = "crates/rargo-purity/allow.txt"


def rows(text):
    return [line.split("\t") for line in text.splitlines() if line.strip()]


def parse_budgets(text):
    suite, default, slow = None, None, {}
    for cols in rows(text):
        if len(cols) != 4 or not cols[2].isdigit() or len(cols[3].strip()) < 10:
            sys.exit(f"{BUDGETS}: want kind, name, milliseconds and a reason separated by tabs: {cols}")
        kind, name, ms = cols[0], cols[1], int(cols[2])
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


def times():
    suite, default, slow = parse_budgets(open(BUDGETS).read())
    took, failed = {}, []
    for line in sys.stdin:
        if not line.startswith("{"):
            continue
        event = json.loads(line)
        if event.get("type") != "test" or event.get("event") not in ("ok", "failed"):
            continue
        name = event["name"]
        if name in took:
            sys.exit(f"two tests are named {name}; budgets name tests, so give one another name")
        took[name] = event.get("exec_time", 0.0) * 1000
        if event["event"] == "failed":
            failed.append(name)
            sys.stdout.write(f"---- {name} ----\n{event.get('stdout', '')}\n")
    problems = [f"{name} failed" for name in failed]
    total = sum(took.values())
    if total > suite:
        problems.append(f"the suite took {total:.0f} ms; its budget is {suite} ms")
    for name, ms in sorted(took.items()):
        limit = slow.get(name, default)
        if ms > limit:
            problems.append(f"{name} took {ms:.0f} ms; its budget is {limit} ms")
    for name, limit in sorted(slow.items()):
        if name not in took:
            problems.append(f"{BUDGETS} gives {name} a budget, but no test of that name ran; remove the line")
        elif took[name] <= default:
            problems.append(f"{name} took {took[name]:.0f} ms, within the default {default} ms; remove its line")
    print(f"{len(took)} tests in {total:.0f} ms of a {suite} ms budget; the slowest took {max(took.values(), default=0):.0f} ms")
    if not took:
        problems.append("no test results were read; run rargo test with -Z unstable-options --format json --report-time")
    for p in problems:
        print(f"FAIL {p}")
    sys.exit(1 if problems else 0)


def at(base, path):
    shown = subprocess.run(["git", "show", f"{base}:{path}"], capture_output=True, text=True)
    return shown.stdout if shown.returncode == 0 else None


def ratchet(base):
    problems = []
    old = at(base, ALLOW)
    if old is not None:
        before, now = parse_allow(old), parse_allow(open(ALLOW).read())
        for key, count in sorted(now.items()):
            if key not in before:
                problems.append(f"{ALLOW} gains an entry: {key[0]} {key[1]}")
            elif count > before[key]:
                problems.append(f"{ALLOW} raises {key[0]} {key[1]} from {before[key]} to {count}")
    old = at(base, BUDGETS)
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
    if sys.argv[1:] == ["times"]:
        times()
    elif len(sys.argv) == 3 and sys.argv[1] == "ratchet":
        ratchet(sys.argv[2])
    else:
        sys.exit("usage: purity.py times < libtest-json | purity.py ratchet BASE")
