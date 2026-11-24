#!/usr/bin/env python3
"""Run the farm-built mount tests on a Linux host with root and /dev/fuse."""
import json
import os
import pathlib
import subprocess
import sys

artifacts = {}
for line in sys.stdin:
    message = json.loads(line)
    if message.get("reason") == "compiler-artifact" and message["executable"]:
        artifacts[message["target"]["name"]] = message["executable"]

sandbox = pathlib.Path(artifacts["rargo-sandbox"]).resolve(strict=True)
for name in ("auto_unmount", "engine"):
    executable = pathlib.Path(artifacts[name]).resolve(strict=True)
    subprocess.run([executable], env={**os.environ, "RARGO_TEST_SANDBOX": str(sandbox)}, check=True)
