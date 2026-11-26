import re
import sys

RUNNING = re.compile(r"^\s*(Running|Doc-tests) (.*?)(?: \((.*)\))?$")
RESULT = re.compile(r"^test (.+) \.\.\. (ok|FAILED|ignored(?:, .*)?)$")


def outcomes(path):
    found = {}
    binary = "?"
    with open(path, errors="replace") as f:
        for line in f:
            line = line.rstrip("\n")
            m = RUNNING.match(line)
            if m:
                binary = f"{m.group(1)} {m.group(2)}"
                continue
            m = RESULT.match(line)
            if m:
                verdict = "ignored" if m.group(2).startswith("ignored") else m.group(2)
                found[(binary, m.group(1))] = verdict
    return found


def main():
    commit, cached_log, real_log = sys.argv[1:4]
    cached, real = outcomes(cached_log), outcomes(real_log)
    diffs = []
    for key in sorted(set(cached) | set(real)):
        a, b = cached.get(key, "absent"), real.get(key, "absent")
        if a != b:
            diffs.append(f"  DIFF {commit} {key[0]} :: {key[1]}: cached {a}, real {b}")
    print(f"{commit} compared={len(set(cached) | set(real))} differences={len(diffs)}")
    for d in diffs:
        print(d)


main()
