"""The watchdog keeps a headless session running for every agent that should
have one. It makes no decisions about work: which issue an agent takes is the
agent's own move, through the ledger.

Each step reconciles the world (the log) with the processes on this machine:

1. A session process that exited is recorded as session_ended, with the
   reason read from its exit (finished, crashed, killed, quota), and quota_out
   if it ran out.
2. An agent the log says is live but that has no process here (the watchdog
   itself restarted, and the process is gone) is recorded as crashed.
3. An agent out of quota is given quota_back when its plan's last quota
   reading says the limit it hit has reset (World.quota_back_at), or after
   quota_retry when there is no reading to go on, so the next step
   tries it: the only way to learn a plan reset is to try.
4. Every agent that is off, has quota, is not backing off from a crash, and
   has work (it still holds an issue, or some issue is ready) gets a worktree
   if it has none, session_started, and a process.
5. A session running longer than the config's session_minutes is killed.

A session is a conversation its harness keeps on disk. The watchdog remembers
which conversation worked on which issue (state/conversations.json). When an
agent starts for an issue it worked on before (one it still holds, or one
that came back and waits for it), the watchdog resumes that conversation and
says what happened, so the agent goes on with what it knew; otherwise the
session is new.

Session processes outlive the watchdog: their pids are kept in state/pids, and
a restarted watchdog adopts the ones still running.
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import signal
import time
import uuid
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
    conv: str | None = None  # its conversation's id, when the watchdog chose it
    seq: int = 0  # the log's last line when it started
    started: datetime | None = None
    issue: int | None = None  # the issue it was started for, if any
    resumed: bool = False
    killed: bool = False  # by the watchdog, for running too long

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
        self.convs: dict[str, dict[str, str]] = {}  # agent -> issue -> conversation id
        try:
            self.convs = json.loads((self.state / "conversations.json").read_text())
        except (OSError, ValueError):
            pass
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
        limit = w.config.dictator.session_minutes
        for agent, s in self.sessions.items():
            if (limit and s.started and not s.killed
                    and self.clock() - s.started > timedelta(minutes=limit)):
                s.killed = True
                self.say(f"{agent}: session over {limit} minutes; killing it")
                try:
                    os.killpg(s.pid, signal.SIGTERM)
                except (ProcessLookupError, PermissionError):
                    pass
        for agent in w.config.names():
            if not w.is_(M.AGENT, agent, AgentS.off) and agent not in self.sessions:
                self._record("session_ended", agent=agent, reason="crashed")
                self.say(f"{agent}: live in the log but has no process here")
        now = self.clock()
        for agent in w.config.names():
            if w.is_(M.QUOTA, agent, "out") and now >= w.quota_back_at(agent, self.quota_retry):
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
            self._beat(every)
            time.sleep(every)

    def _beat(self, every: float) -> None:
        """STATE/watchdog.json: when it last checked and how often, for people
        to see when it next acts. Not the log: a check that did nothing is not
        an event."""
        tmp = self.state / "watchdog.json.tmp"
        tmp.write_text(json.dumps({"t": self.clock().strftime("%Y-%m-%dT%H:%M:%SZ"), "every": every}))
        tmp.replace(self.state / "watchdog.json")

    # ------------------------------------------------------------ sessions

    def _start(self, agent: str) -> None:
        w = self.ledger.world
        t = w.config.type_of(agent)
        issue = w.holding.get(agent)
        if issue is None:
            mine = [n for n in w.ready_queue(agent) if w.reserved_for(n) == agent]
            issue = mine[0] if mine else None
        old = self.convs.get(agent, {}).get(str(issue)) if issue is not None else None
        new = None if old or harness.names_itself(t) else str(uuid.uuid4())
        prompt = self.prompt.replace("{agent}", agent)
        if old:
            prompt = self._wake(agent, issue)
        cmd = harness.command(t, prompt, new=new, resume=old)
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
        s = Session(proc.pid, log, exit_file, proc, conv=old or new, seq=self.ledger.world.seq,
                    started=self.clock(), issue=issue, resumed=bool(old))
        self.sessions[agent] = s
        (self.state / "pids" / agent).write_text(json.dumps({
            "pid": proc.pid, "log": str(log), "exit": str(exit_file), "conv": s.conv, "seq": s.seq,
            "started": s.started.isoformat(), "issue": issue, "resumed": s.resumed}))
        self.say(f"{agent}: started, pid {proc.pid}"
                 + (f", resuming its conversation on #{issue}" if old else ""))

    def _wake(self, agent: str, issue: int) -> str:
        """What a resumed conversation is told: why it is awake again."""
        w = self.ledger.world
        i = w.issues[issue]
        why = "it is yours again"
        for d in reversed(self._lines()):
            ev, about = d["event"], d.get("issue") == issue or (i.pr is not None and d.get("pr") == i.pr)
            if not about:
                continue
            if ev == "ci_done" and d["result"] == "fail":
                why = f"CI failed on pr {i.pr} push {d['push']}: read it with `gh pr checks {i.pr}`"
            elif ev == "review_done" and d["result"] == "changes":
                why = f"{d['by']} asked for changes on pr {i.pr}: read them with `gh pr view {i.pr} --comments`"
            elif ev == "merge_failed":
                why = f"pr {i.pr} did not merge ({d['reason']}): bring it up to date with main"
            elif ev == "diverged":
                why = (f"someone outside the factory pushed to pr {i.pr}: fetch its branch and build on their commits"
                       if d["reason"] == "foreign_push" else f"pr {i.pr} was closed on GitHub without merging")
            elif ev == "unparked":
                why = f"the owner answered on the issue: read it with `gh issue view {issue} --comments`"
            elif ev == "session_ended" or (ev == "returned" and d.get("reason") in ("stale", "quota", "preempted")):
                why = "your last session ended before you finished it"
            else:
                continue
            break
        held = w.holding.get(agent) == issue
        return (f"You are {agent}, awake again in the conversation where you worked on issue #{issue}. "
                f"What happened since: {why}. "
                + ("You still hold it. " if held else
                   f"It waits for you: take it now with `factory submit '{{\"event\":\"taken\",\"issue\":{issue}}}'`. ")
                + "Go on from what you already know, under the same rules as before (AGENT.md in the "
                  "factory): record what you do with `factory submit`, push, and end the session. Never "
                  "wait for CI, review or a person: if you need the owner to decide something, ask on the "
                  "issue and return it as asked.")

    def _lines(self) -> list[dict]:
        try:
            with open(self.ledger.path) as f:
                return [json.loads(x) for x in f.read().splitlines()[-2000:] if x.strip()]
        except OSError:
            return []

    def _ended(self, agent: str, s: Session, code: int) -> None:
        t = self.ledger.world.config.type_of(agent)
        output = s.log.read_text(errors="replace") if s.log.exists() else ""
        reason = m.SessionEnd.killed if s.killed else harness.session_end(t, code, output)
        self._remember(agent, s, harness.conversation(t, output) or s.conv,
                       lost=s.resumed and reason is m.SessionEnd.crashed)
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

    def _remember(self, agent: str, s: Session, conv: str | None, lost: bool) -> None:
        """This conversation is where the agent worked on the issues it
        touched in this session. lost: a resume that crashed; forget it, so
        the next session on that issue is new."""
        mine = self.convs.setdefault(agent, {})
        if lost and s.issue is not None:
            mine.pop(str(s.issue), None)
        elif conv:
            # seq 0: adopted from a watchdog that did not note where it began
            touched = {d["issue"] for d in self._lines()
                       if s.seq and d["seq"] > s.seq and d["actor"] == agent and "issue" in d}
            if s.issue is not None:
                touched.add(s.issue)
            for n in touched:
                mine[str(n)] = conv
        w = self.ledger.world
        for n in [n for n in mine if int(n) in w.issues and not w.open_(int(n))]:
            del mine[n]  # closed: nothing comes back
        tmp = self.state / "conversations.json.tmp"
        tmp.write_text(json.dumps(self.convs, indent=1))
        tmp.replace(self.state / "conversations.json")

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
            self.sessions[f.name] = Session(
                d["pid"], Path(d["log"]), exit_file, None, conv=d.get("conv"), seq=d.get("seq", 0),
                started=datetime.fromisoformat(d["started"]) if d.get("started") else None,
                issue=d.get("issue"), resumed=d.get("resumed", False))

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
