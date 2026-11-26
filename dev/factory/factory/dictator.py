"""The dictator. Every tick_minutes it:

1. takes back issues whose holder's session ended (stale) or whose holder is
   out of quota;
2. if a bug has been ready longer than preempt_minutes and no agent is free,
   takes the most recently taken non-bug issue back from its holder;
3. closes initiatives whose children are all closed, in the log and on
   GitHub.

All three are decisions inside one tick. Then, outside the tick, it plans: an
open initiative with no children gets a planning session (a harness run with
DICTATOR.md as its prompt) that files sub-issues on GitHub, which the observer
records as the dictator's. The dictator writes no code.

Every orchestrate_minutes it also orchestrates: a session with ORCHESTRATE.md
as its prompt looks at the whole factory and decides what the rules above
cannot: the order of work, duplicates, missing or oversized issues, guidance
for agents. It runs inside a tick of its own, so what it decides is checked
and recorded like any other dictator decision, and it keeps notes between
runs in CHECKOUT/orchestration.md. Both kinds of session run on the config's
dictator.model.
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
from .github import GitHub, GitHubError
from .ledger import Ledger, Refused, utc_now
from .machines import AgentS, IssueS
from .notify import notify
from .model import Kind, Role
from .watchdog import BIN


class Dictator:
    def __init__(self, ledger: Ledger, gh: GitHub, workdir: Path, prompt: str,
                 clock: Callable[[], datetime] = utc_now,
                 plan_timeout: timedelta = timedelta(minutes=45),
                 replan_after: timedelta = timedelta(hours=24),
                 run: Callable[..., int] | None = None,
                 say: Callable[[str], None] = print,
                 orchestrate_prompt: str | None = None, state: Path | None = None,
                 answer_prompt: str | None = None):
        self.answer_prompt = answer_prompt
        self.answering: dict[int, int] = {}  # asked issue -> sessions tried
        self.ledger, self.gh, self.workdir, self.prompt = ledger, gh, Path(workdir), prompt
        self.clock, self.plan_timeout, self.replan_after = clock, plan_timeout, replan_after
        self.run_session = run or self._run_session
        self.say = say
        self.orchestrate_prompt, self.state = orchestrate_prompt, state
        self.orchestrated: datetime | None = None  # when the last run began
        notes = self.workdir / "orchestration.md"
        if notes.exists():  # a restarted dictator does not orchestrate again at once
            self.orchestrated = datetime.fromtimestamp(notes.stat().st_mtime, self.clock().tzinfo)
        self.planned: dict[int, datetime] = {}  # initiative -> when we last tried
        self.told: set[int] = set()  # issues every agent gave up on, owner told
        self.synced: set[int] = set()  # agent closes checked against GitHub

    def tick(self) -> None:
        w = self.ledger.refresh()
        if w.is_(M.DICTATOR, "", "ticking"):  # a dictator stopped mid-tick left it open
            self._decide("tick_ended", tick=w.tick)
            w = self.ledger.refresh()
        n = w.tick + 1
        self._decide("tick_started", tick=n)
        try:
            self._take_back()
            self._preempt()
            self._close_initiatives()
        finally:
            self._decide("tick_ended", tick=n)
        self._stuck()
        self._sync_closed()
        self._plan()
        self._answer()
        self._orchestrate()

    def _sync_closed(self) -> None:
        """Close on GitHub, once, what an agent closed in the log (it should
        have closed it there itself)."""
        w = self.ledger.world
        mine = {n for n, i in w.issues.items() if i.closer in w.config.names()
                and not w.open_(n) and n not in self.synced}
        if not mine:
            return
        try:
            still = {i["number"] for i in self.gh.issues(None)} & mine
            for n in still:
                self.gh.close(n, "completed", f"Closed by {w.issues[n].closer}: see the factory log.")
        except GitHubError as err:
            self.say(f"dictator: closing on GitHub: {err}")
            return
        self.synced |= mine

    def _stuck(self) -> None:
        """Work no agent will do is the dictator's to rethink: orchestrate at
        once. Without an orchestrator, tell the owner, once."""
        w = self.ledger.world
        agents = set(w.config.names())
        for n, i in w.issues.items():
            if w.waiting(n) and agents <= i.given_up and n not in self.told:
                self.told.add(n)
                if w.config.dictator.orchestrate_minutes and self.orchestrate_prompt:
                    self.orchestrated = None
                    self.say(f"dictator: every agent gave up on #{n}; orchestrating now")
                    continue
                notify(f"#{n}: every agent gave up on it. Put it on hold and release it "
                       f"(label guard, then remove it) to let them try again.",
                       "rargo factory: needs the owner")
                self.say(f"dictator: #{n} needs the owner; every agent gave up on it")

    def _answer(self) -> None:
        """An agent that asked gets its answer from the dictator: a session per
        question, inside a tick, that decides, comments, and releases the
        issue. Two tries; then the owner is told."""
        if self.answer_prompt is None:
            return
        for n in [n for n, i in self.ledger.refresh().issues.items() if i.asked]:
            tries = self.answering.get(n, 0)
            if tries >= 2:
                if tries == 2:
                    self.answering[n] = 3
                    notify(f"#{n}: an agent asked a question and the dictator did not settle it.",
                           "rargo factory: unanswered question")
                continue
            self.answering[n] = tries + 1
            if not self._decide("tick_started", tick=self.ledger.world.tick + 1):
                return
            self.say(f"dictator: answering the question on #{n}")
            try:
                code = self.run_session(n, self.answer_prompt.replace("{issue}", str(n)))
            finally:
                self._decide("tick_ended", tick=self.ledger.refresh().tick)
            self.say(f"dictator: answering #{n} ended with exit {code}")
            if not self.ledger.world.issues[n].asked:
                self.answering.pop(n, None)

    def run(self) -> None:
        while True:
            began = time.monotonic()
            self.tick()  # a tick every tick_minutes, however long this one took
            time.sleep(max(10.0, self.ledger.world.config.dictator.tick_minutes * 60
                           - (time.monotonic() - began)))

    # ------------------------------------------------------------ decisions

    def _take_back(self) -> None:
        w = self.ledger.world
        stuck = w.config.dictator.no_progress_sessions
        for agent, issue in list(w.holding.items()):
            if (stuck and w.is_(M.AGENT, agent, AgentS.off)
                    and w.issues[issue].stuck.get(agent, 0) >= stuck):
                self._decide("returned", issue=issue, reason="no_progress")
            elif w.is_(M.AGENT, agent, AgentS.off):
                self._decide("returned", issue=issue, reason="stale")
            elif w.is_(M.QUOTA, agent, "out"):
                self._decide("returned", issue=issue, reason="quota")

    def _preempt(self) -> None:
        w = self.ledger.world
        limit = w.config.dictator.preempt_minutes
        if limit is None or w.free_agents():
            return
        cutoff = w.t - timedelta(minutes=limit)
        if not any(i.kind is Kind.bug and w.is_(M.ISSUE, n, IssueS.ready)
                   and w.since(M.ISSUE, n) <= cutoff for n, i in w.issues.items()):
            return
        held = [n for n in w.holding.values() if w.issues[n].kind is not Kind.bug]
        if held:  # least far in: taken most recently
            n = max(held, key=lambda n: w.since(M.ISSUE, n))
            self._decide("returned", issue=n, reason="preempted",
                         note="a bug has waited longer than the preemption limit")

    def _close_initiatives(self) -> None:
        w = self.ledger.world
        for n, i in w.issues.items():
            if (i.kind is Kind.initiative and w.open_(n) and i.children
                    and not any(w.open_(c) for c in i.children)):
                if self._decide("closed", issue=n, reason="done"):
                    try:
                        self.gh.close(n, "completed", "Every sub-issue is closed.")
                    except GitHubError as err:
                        self.say(f"#{n}: closed in the factory, not on GitHub: {err}")

    def _decide(self, event: str, **fields) -> bool:
        try:
            self.ledger.submit(Role.dictator, json.dumps({"event": event, **fields}))
            return True
        except Refused as err:
            self.say(f"dictator: {err}")
            return False

    # ------------------------------------------------------------ planning

    def _plan(self) -> None:
        w = self.ledger.refresh()
        now = self.clock()
        for n, i in sorted(w.issues.items()):
            if (i.kind is Kind.initiative and w.open_(n) and not i.children and not i.parked
                    and now - self.planned.get(n, now - self.replan_after) >= self.replan_after):
                self.planned[n] = now
                self.say(f"dictator: planning initiative #{n}")
                code = self.run_session(n)
                self.say(f"dictator: planning #{n} ended with exit {code}")

    def _orchestrate(self) -> None:
        w = self.ledger.refresh()
        every = w.config.dictator.orchestrate_minutes
        now = self.clock()
        if every is None or self.orchestrate_prompt is None:
            return
        if self.orchestrated and now - self.orchestrated < timedelta(minutes=every, seconds=-30):
            return
        self.orchestrated = now
        notes = self.workdir / "orchestration.md"
        prompt = (self.orchestrate_prompt.replace("{notes}", str(notes))
                  .replace("{state}", str(self.state or self.workdir)))
        if not self._decide("tick_started", tick=w.tick + 1):
            return
        self.say("dictator: orchestrating")
        try:  # inside a tick: its decisions are the dictator's
            code = self.run_session("orchestrate", prompt)
        finally:
            self._decide("tick_ended", tick=self.ledger.refresh().tick)
        self.say(f"dictator: orchestrating ended with exit {code}")

    def _run_session(self, initiative: int | str, prompt: str | None = None) -> int:
        w = self.ledger.world
        t = next((a for a in w.config.agents if a.type is m.Harness.claude_code),
                 w.config.agents[0])
        if w.config.dictator.model is not None:
            t = t.model_copy(update={"model": w.config.dictator.model})
        cmd = harness.command(t, prompt or self.prompt.replace("{initiative}", str(initiative)))
        if cmd is None:
            self.say(f"dictator: no {t.type} here to plan with")
            return 127
        argv, extra = cmd
        self.workdir.mkdir(parents=True, exist_ok=True)
        if (self.workdir / ".git").exists():  # plan against today's main
            subprocess.run(["git", "-C", str(self.workdir), "pull", "-q", "--ff-only"],
                           capture_output=True)
        log = self.workdir / f"plan-{initiative}-{self.clock():%Y%m%dT%H%M%S}.log"
        env = os.environ | extra | {
            "FACTORY_LOG": self.ledger.path, "FACTORY_ACTOR": Role.dictator,
            "FACTORY_PYTHON": sys.executable,
            "PATH": f"{BIN}{os.pathsep}{os.environ.get('PATH', '')}"}
        with open(log, "wb") as out:
            try:
                return subprocess.run(argv, cwd=self.workdir, env=env, stdin=subprocess.DEVNULL,
                                      stdout=out, stderr=subprocess.STDOUT,
                                      timeout=self.plan_timeout.total_seconds()).returncode
            except subprocess.TimeoutExpired:
                return 124
