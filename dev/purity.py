import subprocess
import sys

ALLOW = "crates/rargo-purity/allow.txt"
SCANNER = "crates/rargo-purity/src/"
PURITY = "crates/rargo-purity/"


def rows(text):
    return [line.split("\t") for line in text.splitlines() if line.strip()]


def parse_allow(text):
    return {(c[0], c[1]): int(c[2]) for c in rows(text)}


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
    for p in problems:
        print(f"FAIL {p}")
    print("the allowlist only shrinks" if not problems else f"{len(problems)} increases against {base}")
    sys.exit(1 if problems else 0)


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "ratchet":
        ratchet(sys.argv[2])
    else:
        sys.exit("usage: purity.py ratchet BASE")
