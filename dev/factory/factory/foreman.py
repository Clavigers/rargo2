"""The foreman. Every tick_minutes it:

1. takes back issues whose holder's session ended (stale) or whose holder is
   out of quota;
2. if a bug has been ready longer than preempt_minutes and no agent is free,
   takes the most recently taken non-bug issue back from its holder;
3. closes initiatives whose children are all closed, in the log and on
   GitHub.

All three are decisions inside one tick. Then, outside the tick, it plans: an
open initiative with no children gets a planning session (a harness run with
FOREMAN.md as its prompt) that files sub-issues on GitHub, which the observer
records as the foreman's. The foreman writes no code.

Every orchestrate_minutes it also orchestrates: a session with ORCHESTRATE.md
as its prompt looks at the whole factory and decides what the rules above
cannot: the order of work, duplicates, missing or oversized issues, guidance
for agents. It runs inside a tick of its own, so what it decides is checked
and recorded like any other foreman decision, and it keeps notes between
runs in CHECKOUT/orchestration.md. Both kinds of session run on the config's
foreman.model.
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


class Foreman:
    def __init__(self, ledger: Ledger, gh: GitHub, workdir: Path, prompt: str,
                 clock: Callable[[], datetime] = utc_now,
                 plan_timeout: timedelta = timedelta(minutes=45),
                 replan_after: timedelta = timedelta(hours=24),
                 run: Callable[..., int] | None = None,
                 say: Callable[[str], None] = print,
                 orchestrate_prompt: str | None = None, state: Path | None = None):
        self.ledger, self.gh, self.workdir, self.prompt = ledger, gh, Path(workdir), prompt
        self.clock, self.plan_timeout, self.replan_after = clock, plan_timeout, replan_after
        self.run_session = run or self._run_session
        self.say = say
        self.orchestrate_prompt, self.state = orchestrate_prompt, state
        self.orchestrated: datetime | None = None  # when the last run began
        self.planned: dict[int, datetime] = {}  # initiative -> when we last tried
        self.told: set[int] = set()  # issues every agent gave up on, owner told
        self.synced: set[int] = set()  # agent closes checked against GitHub

    def tick(self) -> None:
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
            self.say(f"foreman: closing on GitHub: {err}")
            return
        self.synced |= mine

    def _stuck(self) -> None:
        """Tell the owner, once, about work no agent will do."""
        w = self.ledger.world
        agents = set(w.config.names())
        for n, i in w.issues.items():
            if w.waiting(n) and agents <= i.given_up and n not in self.told:
                self.told.add(n)
                notify(f"#{n}: every agent gave up on it. Put it on hold and release it "
                       f"(label guard, then remove it) to let them try again.",
                       "rargo factory: needs the owner")
                self.say(f"foreman: #{n} needs the owner; every agent gave up on it")

    def run(self) -> None:
        while True:
            self.tick()
            time.sleep(self.ledger.world.config.foreman.tick_minutes * 60)

    # ------------------------------------------------------------ decisions

    def _take_back(self) -> None:
        w = self.ledger.world
        stuck = w.config.foreman.no_progress_sessions
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
        limit = w.config.foreman.preempt_minutes
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
            self.ledger.submit(Role.foreman, json.dumps({"event": event, **fields}))
            return True
        except Refused as err:
            self.say(f"foreman: {err}")
            return False

    # ------------------------------------------------------------ planning

    def _plan(self) -> None:
        w = self.ledger.refresh()
        now = self.clock()
        for n, i in sorted(w.issues.items()):
            if (i.kind is Kind.initiative and w.open_(n) and not i.children and not i.parked
                    and now - self.planned.get(n, now - self.replan_after) >= self.replan_after):
                self.planned[n] = now
                self.say(f"foreman: planning initiative #{n}")
                code = self.run_session(n)
                self.say(f"foreman: planning #{n} ended with exit {code}")

    def _orchestrate(self) -> None:
        w = self.ledger.refresh()
        every = w.config.foreman.orchestrate_minutes
        now = self.clock()
        if every is None or self.orchestrate_prompt is None:
            return
        if self.orchestrated and now - self.orchestrated < timedelta(minutes=every):
            return
        self.orchestrated = now
        notes = self.workdir / "orchestration.md"
        prompt = (self.orchestrate_prompt.replace("{notes}", str(notes))
                  .replace("{state}", str(self.state or self.workdir)))
        if not self._decide("tick_started", tick=w.tick + 1):
            return
        self.say("foreman: orchestrating")
        try:  # inside a tick: its decisions are the foreman's
            code = self.run_session("orchestrate", prompt)
        finally:
            self._decide("tick_ended", tick=self.ledger.refresh().tick)
        self.say(f"foreman: orchestrating ended with exit {code}")

    def _run_session(self, initiative: int | str, prompt: str | None = None) -> int:
        w = self.ledger.world
        t = next((a for a in w.config.agents if a.type is m.Harness.claude_code),
                 w.config.agents[0])
        if w.config.foreman.model is not None:
            t = t.model_copy(update={"model": w.config.foreman.model})
        cmd = harness.command(t, prompt or self.prompt.replace("{initiative}", str(initiative)))
        if cmd is None:
            self.say(f"foreman: no {t.type} here to plan with")
            return 127
        argv, extra = cmd
        self.workdir.mkdir(parents=True, exist_ok=True)
        if (self.workdir / ".git").exists():  # plan against today's main
            subprocess.run(["git", "-C", str(self.workdir), "pull", "-q", "--ff-only"],
                           capture_output=True)
        log = self.workdir / f"plan-{initiative}-{self.clock():%Y%m%dT%H%M%S}.log"
        env = os.environ | extra | {
            "FACTORY_LOG": self.ledger.path, "FACTORY_ACTOR": Role.foreman,
            "FACTORY_PYTHON": sys.executable,
            "PATH": f"{BIN}{os.pathsep}{os.environ.get('PATH', '')}"}
        with open(log, "wb") as out:
            try:
                return subprocess.run(argv, cwd=self.workdir, env=env, stdin=subprocess.DEVNULL,
                                      stdout=out, stderr=subprocess.STDOUT,
                                      timeout=self.plan_timeout.total_seconds()).returncode
            except subprocess.TimeoutExpired:
                return 124
