"""The watchdog keeps a headless session running for every agent that should
have one. It makes no decisions about work: which issue an agent takes is the
agent's own move, through the ledger.

Each step reconciles the world (the log) with the processes on this machine:

1. A session process that exited is recorded as session_ended, with the
   reason read from its exit (finished, crashed, killed, quota), and quota_out
   if it ran out.
2. An agent the log says is live but that has no process here (the watchdog
   itself restarted, and the process is gone) is recorded as crashed.
3. An agent out of quota for quota_retry is given quota_back, so the next step
   tries it: the only way to learn a plan reset is to try.
4. Every agent that is off, has quota, is not backing off from a crash, and
   has work (it still holds an issue, or some issue is ready) gets a worktree
   if it has none, session_started, and a process.

Session processes outlive the watchdog: their pids are kept in state/pids, and
a restarted watchdog adopts the ones still running.
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import time
from dataclasses import dataclass
from datetime import datetime, timedelta
from pathlib import Path
from typing import Callable

from . import harness
from . import machines as M
from . import model as m
from .ledger import Alarm, Ledger, utc_now
from .machines import AgentS
from .notify import notify

BIN = Path(__file__).resolve().parent.parent / "bin"


# Runs the harness and leaves its exit code in $FACTORY_EXIT, so a watchdog
# that adopted the session (and so cannot wait for it) still learns how it
# ended. A signal shows as 128 + its number.
WRAP = '"$@"; c=$?; echo $c > "$FACTORY_EXIT.tmp" && mv "$FACTORY_EXIT.tmp" "$FACTORY_EXIT"; exit $c'


@dataclass
class Session:
    pid: int
    log: Path
    exit: Path  # where the wrapper writes the exit code
    proc: subprocess.Popen | None  # None when adopted from an earlier watchdog

    def exit_code(self) -> int | None:
        if self.proc is not None:
            return self.proc.poll()
        try:
            os.kill(self.pid, 0)
            return None
        except ProcessLookupError:
            try:
                return int(self.exit.read_text())
            except (OSError, ValueError):
                return 1  # gone without a word: killed with its wrapper


class Watchdog:
    def __init__(self, ledger: Ledger, repo: Path, state: Path, prompt: str,
                 base: str = "origin/main",
                 quota_retry: timedelta = timedelta(minutes=30),
                 clock: Callable[[], datetime] = utc_now,
                 say: Callable[[str], None] = print):
        self.ledger, self.repo, self.state = ledger, Path(repo), Path(state)
        self.prompt, self.base, self.quota_retry = prompt, base, quota_retry
        self.clock, self.say = clock, say
        self.sessions: dict[str, Session] = {}
        self.backoff: dict[str, tuple[int, datetime]] = {}  # agent -> (crashes, not before)
        for d in ("pids", "logs", "worktrees", "exits"):
            (self.state / d).mkdir(parents=True, exist_ok=True)
        self._adopt()

    # ------------------------------------------------------------ one step

    def step(self) -> None:
        w = self.ledger.refresh()
        assert w.config, "the log has no config yet"
        for agent, s in list(self.sessions.items()):
            code = s.exit_code()
            if code is not None:
                self._ended(agent, s, code)
        w = self.ledger.world
        for agent in w.config.names():
            if not w.is_(M.AGENT, agent, AgentS.off) and agent not in self.sessions:
                self._record("session_ended", agent=agent, reason="crashed")
                self.say(f"{agent}: live in the log but has no process here")
        now = self.clock()
        for agent in w.config.names():
            if w.is_(M.QUOTA, agent, "out") and now - w.since(M.QUOTA, agent) >= self.quota_retry:
                self._record("quota_back", agent=agent)
        w = self.ledger.world
        for agent in w.config.names():
            crashes, not_before = self.backoff.get(agent, (0, now))
            work = agent in w.holding or w.ready_queue(agent)  # else a session would only end
            if (work and w.is_(M.AGENT, agent, AgentS.off) and w.is_(M.QUOTA, agent, "ok")
                    and agent not in self.sessions and now >= not_before):
                self._start(agent)

    def run(self, every: float = 30.0) -> None:
        while True:
            self.step()
            time.sleep(every)

    # ------------------------------------------------------------ sessions

    def _start(self, agent: str) -> None:
        w = self.ledger.world
        t = w.config.type_of(agent)
        cmd = harness.command(t, self.prompt.replace("{agent}", agent))
        if cmd is None:
            return  # this harness is not installed on this machine
        argv, extra = cmd
        wt = self._worktree(agent)
        if not self._record("session_started", agent=agent):
            return
        log = self.state / "logs" / f"{agent}-{self.clock():%Y%m%dT%H%M%S}.log"
        exit_file = self.state / "exits" / agent
        exit_file.unlink(missing_ok=True)
        env = os.environ | extra | {
            "FACTORY_EXIT": str(exit_file),
            "FACTORY_LOG": self.ledger.path, "FACTORY_ACTOR": agent,
            "FACTORY_PYTHON": sys.executable,
            "PATH": f"{BIN}{os.pathsep}{os.environ.get('PATH', '')}"}
        try:
            with open(log, "wb") as out:
                proc = subprocess.Popen(["sh", "-c", WRAP, "sh", *argv], cwd=wt, env=env,
                                        stdin=subprocess.DEVNULL,
                                        stdout=out, stderr=subprocess.STDOUT,
                                        start_new_session=True)
        except OSError as err:
            self.say(f"{agent}: could not start: {err}")
            self._record("session_ended", agent=agent, reason="crashed")
            self._crashed(agent)
            return
        self.sessions[agent] = Session(proc.pid, log, exit_file, proc)
        (self.state / "pids" / agent).write_text(
            json.dumps({"pid": proc.pid, "log": str(log), "exit": str(exit_file)}))
        self.say(f"{agent}: started, pid {proc.pid}")

    def _ended(self, agent: str, s: Session, code: int) -> None:
        t = self.ledger.world.config.type_of(agent)
        output = s.log.read_text(errors="replace") if s.log.exists() else ""
        reason = harness.session_end(t, code, output)
        del self.sessions[agent]
        (self.state / "pids" / agent).unlink(missing_ok=True)
        self._record("session_ended", agent=agent, reason=reason.value)
        self.say(f"{agent}: session ended ({reason}, exit {code})")
        if reason is m.SessionEnd.quota:
            self._record("quota_out", agent=agent)
        if reason is m.SessionEnd.crashed:
            self._crashed(agent)
        else:
            self.backoff.pop(agent, None)

    def _crashed(self, agent: str) -> None:
        """Back off 1, 2, 4 … 32 minutes after crashes in a row, so a harness
        that dies at once does not spin."""
        crashes = self.backoff.get(agent, (0, self.clock()))[0] + 1
        delay = timedelta(minutes=min(2 ** (crashes - 1), 32))
        self.backoff[agent] = (crashes, self.clock() + delay)

    def _adopt(self) -> None:
        for f in (self.state / "pids").iterdir():
            d = json.loads(f.read_text())
            exit_file = Path(d.get("exit", self.state / "exits" / f.name))
            self.sessions[f.name] = Session(d["pid"], Path(d["log"]), exit_file, None)

    def _worktree(self, agent: str) -> Path:
        wt = self.state / "worktrees" / agent
        if not wt.exists():
            subprocess.run(["git", "-C", str(self.repo), "fetch", "-q", "origin"],
                           check=False, capture_output=True)
            subprocess.run(["git", "-C", str(self.repo), "worktree", "add", "-q", "--detach",
                            str(wt), self.base], check=True, capture_output=True)
        return wt

    def _record(self, event: str, **fields) -> bool:
        try:
            self.ledger.submit(m.Role.watchdog, json.dumps({"event": event, **fields}))
            return True
        except Alarm as err:
            self.say(f"ALARM {err}")
            notify(f"watchdog: {err}", "rargo factory alarm")
            return False
