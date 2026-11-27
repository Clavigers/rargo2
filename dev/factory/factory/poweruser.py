"""The power user: it uses each release candidate the way a person would,
and files what goes wrong.

Every few minutes it looks for a use that is due: a candidate whose latest
build has had fewer power user sessions end on it than the config's
uses_required, with no backport waiting for a build. Then it

1. records `use_started` for that build;
2. checks the build's commit out in CHECKOUT and runs one harness session
   there with POWER-USER.md as its prompt. The session builds the candidate,
   uses it, and files bugs and friction on GitHub; the observer records them
   as found in this use;
3. waits long enough for the observer to have seen those issues, and records
   `use_ended`.

Sessions are at least `gap` apart, so a week of small backports does not
become a week of sessions. A use left open by a power user that stopped is
ended when the next one starts. It writes no code and touches no pull
request.
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import time
from datetime import datetime, timedelta
from pathlib import Path
from typing import Callable

from . import harness
from . import machines as M
from . import model as m
from .ledger import Alarm, Ledger, Refused, utc_now
from .model import Role
from .watchdog import BIN


class PowerUser:
    def __init__(self, ledger: Ledger, checkout: Path, prompt: str,
                 clock: Callable[[], datetime] = utc_now,
                 gap: timedelta = timedelta(hours=3),
                 timeout: timedelta = timedelta(minutes=90),
                 settle: float = 150.0,
                 run: Callable[..., int] | None = None,
                 say: Callable[[str], None] = print):
        self.ledger, self.checkout, self.prompt = ledger, Path(checkout), prompt
        self.clock, self.gap, self.timeout, self.settle = clock, gap, timeout, settle
        self.run_session = run or self._run_session
        self.say = say

    def run(self, every: float = 300.0) -> None:
        while True:
            try:
                self.step()
            except (subprocess.CalledProcessError, Alarm, Refused) as err:
                self.say(f"power user: {err}")  # the next step tries again
            time.sleep(every)

    def due(self) -> tuple[str, int, str] | None:
        """The version, build and commit to use now, if any."""
        w = self.ledger.refresh()
        if w.config is None:
            return None
        mine = {n: u for n, u in w.uses.items() if u.by == Role.power_user}
        last = max((w.since(M.USE, n) for n in mine), default=None)
        if last and self.clock() - last < self.gap:
            return None
        for version, v in w.versions.items():
            build = len(v.builds)
            if not w.is_(M.VERSION, version, "candidate") or build == 0 or v.unbuilt:
                continue
            ended = sum(1 for n, u in mine.items() if (u.version, u.build) == (version, build)
                        and w.is_(M.USE, n, "ended"))
            if ended < w.config.release.uses_required:
                sha = [l["sha"] for l in self._lines("built") if l["version"] == version][-1]
                return version, build, sha
        return None

    def step(self) -> None:
        w = self.ledger.refresh()
        for n, u in w.uses.items():  # left open by a power user that stopped
            if u.by == Role.power_user and w.is_(M.USE, n, "open"):
                self._record("use_ended", use=n)
        d = self.due()
        if d is None:
            return
        version, build, sha = d
        use = max(self.ledger.world.uses, default=0) + 1
        self._record("use_started", use=use, version=version, build=build)
        self.say(f"power user: use {use}: {version} build {build} at {sha[:7]}")
        try:
            code = self.run_session(use, version, build, sha)
            self.say(f"power user: use {use} ended with exit {code}")
            time.sleep(self.settle)  # the observer records what it filed, in this use
        finally:
            self._record("use_ended", use=use)

    def _run_session(self, use: int, version: str, build: int, sha: str) -> int:
        w = self.ledger.world
        t = next((a for a in w.config.agents if a.type is m.Harness.claude_code), w.config.agents[0])
        if w.config.release.use_model is not None:
            t = t.model_copy(update={"model": w.config.release.use_model})
        prompt = (self.prompt.replace("{version}", version).replace("{build}", str(build))
                  .replace("{sha}", sha).replace("{use}", str(use)))
        cmd = harness.command(t, prompt)
        if cmd is None:
            self.say(f"power user: no {t.type} here to run a session with")
            return 127
        argv, extra = cmd
        git = ["git", "-C", str(self.checkout)]
        subprocess.run([*git, "fetch", "-q", "--tags", "origin"], check=True, capture_output=True)
        subprocess.run([*git, "checkout", "-q", "--detach", "-f", sha], check=True, capture_output=True)
        env = os.environ | extra | {
            "FACTORY_LOG": self.ledger.path, "FACTORY_ACTOR": Role.power_user,
            "FACTORY_PYTHON": sys.executable,
            "PATH": f"{BIN}{os.pathsep}{os.environ.get('PATH', '')}"}
        log = self.checkout.parent / "state" / "logs"
        log.mkdir(parents=True, exist_ok=True)
        with open(log / f"use-{use}.log", "wb") as out:
            try:
                return subprocess.run(argv, cwd=self.checkout, env=env, stdin=subprocess.DEVNULL,
                                      stdout=out, stderr=subprocess.STDOUT,
                                      timeout=self.timeout.total_seconds()).returncode
            except subprocess.TimeoutExpired:
                return 124

    def _record(self, event: str, **fields) -> None:
        self.ledger.submit(Role.power_user, json.dumps({"event": event, **fields}))

    def _lines(self, event: str) -> list[dict]:
        key = f'"event":"{event}"'
        with open(self.ledger.path) as f:
            return [json.loads(l) for l in f if key in l]
