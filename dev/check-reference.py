#!/usr/bin/env python3
"""Check Rargo CLI examples against the client and server of this commit."""
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CLIENT = ROOT / "crates/rargo/src"
client_main = (CLIENT / "main.rs").read_text()
client_env = set()
for path in CLIENT.glob("*.rs"):
    client_env.update(re.findall(r'"(RARGO_[A-Z0-9_]+)"', path.read_text()))

rargo, rargod = (str(Path(arg).resolve()) for arg in sys.argv[1:])
client_help = subprocess.check_output([rargo, "auth", "--help"], text=True)
# The token parser prints its usage when called without a state directory.
server_help = subprocess.run(
    [rargod, "token"], capture_output=True, text=True, check=False
).stderr
routed = re.search(r'const ROUTED: &\[&str\]\s*=\s*&\[(.*?)\];', client_main, re.DOTALL)
if routed is None:
    sys.exit("could not find the routed command list in main.rs")
client_commands = set(re.findall(r'"([a-z-]+)"', routed.group(1)))
if 'Some("clean") =>' in client_main:
    client_commands.add("clean")
client_flags = set(re.findall(r'"(--rargo-[a-z-]+)"', client_main))

doc = (ROOT / "docs/reference.md").read_text()
commands = re.findall(r'`((?:rargo|rargod) [^`\n]+)`', doc)
commands += re.findall(r'<cmd>((?:rargo|rargod) [^<]+)</cmd>', doc)
for shell in re.findall(r'^```sh\n(.*?)^```', doc, re.M | re.S):
    for line in shell.splitlines():
        if re.match(r'^rargod? ', line):
            commands.append(re.split(r'\s+#', line, maxsplit=1)[0])

errors = []
for command in commands:
    program, verb, *args = command.split()
    options = [arg for arg in args if arg.startswith("--")]
    if verb.startswith("@") and program == "rargo":
        recognized = "strip_prefix('@')" in client_main
    elif verb == "auth" and program == "rargo":
        prefix = "rargo auth"
        if args and not args[0].startswith("--"):
            prefix += f" {args[0]}"
        recognized = any(
            line == prefix or line.startswith(prefix + " ")
            for line in map(str.strip, client_help.splitlines())
            if all(option in line.split() or f"[{option}" in line for option in options)
        )
    elif verb == "token" and program == "rargod" and args:
        recognized = all(
            re.search(rf'\brargod token {re.escape(subcommand)}\s', server_help)
            and all(re.search(rf'\brargod token {re.escape(subcommand)}[^\n]*{re.escape(option)}\b', server_help)
                    for option in options)
            for subcommand in args[0].split("|")
        )
    elif verb == "--version" and program == "rargod":
        recognized = subprocess.run([rargod, verb], capture_output=True, check=False).returncode == 0
    else:
        recognized = program == "rargo" and not options and (verb in client_commands or verb in client_flags)
    if not recognized:
        errors.append(f"unknown example: {command}")

for name in sorted(set(re.findall(r'\bRARGO_[A-Z0-9_]+\b', doc)) - client_env):
    errors.append(f"unknown environment option: {name}")

if errors:
    sys.exit("\n".join(errors))
print(f"checked {len(commands)} Rargo command references and the environment options")
