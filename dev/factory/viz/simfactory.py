"""A SimPy model of Factory2 that writes the same log the real factory does.

Every role of the real factory is a SimPy process here, doing what its
service does: the human files work, the foreman ticks, the watchdog keeps
sessions, agents do one issue per session, CI and the reviewers judge
pushes, and the merger merges. Each act is an event of factory.model,
applied to a factory.world.World, so the simulation can only make moves the
real checker allows: a refused move stops it with the reason, because then
the model is wrong. The visualizer reads its lines exactly as it reads the
real log.

Time is in minutes from START. Rates and odds are parameters (PARAMS), set
from the GUI; the defaults are guesses, marked as such.
"""
from __future__ import annotations

import json
import pathlib
import random
from datetime import datetime, timedelta, timezone

import simpy

from factory import model as m
from factory import machines as M
from factory.machines import AgentS, IssueS
from factory.world import World, green

START = datetime(2026, 12, 1, tzinfo=timezone.utc)
DAY = 1440
CONFIG = pathlib.Path(__file__).parent.parent / "deploy" / "config.json"

# key, group, label, min, max, step, default
PARAMS = [
    ("issues_per_day", "Human", "Issues filed per day", 0, 40, 1, 6),
    ("bug_share", "Human", "Share of them that are bugs (%)", 0, 100, 5, 30),
    ("friction_share", "Human", "Share that are friction (%)", 0, 100, 5, 15),
    ("initiatives_per_week", "Human", "Initiatives per week", 0, 7, 0.5, 1),
    ("hold_odds", "Human", "Issues the owner holds a while (%)", 0, 50, 1, 5),
    ("children", "Foreman", "Issues per planned initiative", 1, 20, 1, 6),
    ("plan_minutes", "Foreman", "Planning session (min)", 5, 240, 5, 40),
    ("claude-code", "Agents", "Claude Code agents", 0, 8, 1, 1),
    ("codex", "Agents", "Codex agents", 0, 8, 1, 1),
    ("grok", "Agents", "Grok agents", 0, 8, 1, 1),
    ("work_minutes", "Agents", "Median minutes per issue", 2, 240, 1, 25),
    ("quota_per_hour", "Agents", "Chance per working hour of running out (%)", 0, 100, 1, 8),
    ("quota_rest", "Agents", "Out of quota for (min)", 30, 600, 30, 300),
    ("give_up", "Agents", "Issues given up (%)", 0, 50, 1, 6),
    ("blocked", "Agents", "Issues found blocked (%)", 0, 50, 1, 4),
    ("verified", "Agents", "Issues closed with nothing to change (%)", 0, 50, 1, 5),
    ("finds_bug", "Agents", "Sessions that file a bug they found (%)", 0, 50, 1, 8),
    ("crash", "Agents", "Sessions that crash (%)", 0, 30, 1, 2),
    ("ci_minutes", "Checks", "CI run (min)", 1, 60, 1, 4),
    ("ci_fail", "Checks", "CI failures (%)", 0, 80, 1, 12),
    ("review_mux", "Checks", "Use review-mux instead of the placeholder (0/1)", 0, 1, 1, 0),
    ("review_minutes", "Checks", "review-mux review (min)", 1, 60, 1, 3),
    ("review_changes", "Checks", "review-mux asks for changes (%)", 0, 80, 1, 25),
    ("conflict", "Checks", "Merges that conflict (%)", 0, 50, 1, 3),
]
GUESSES = {k for k, *_ in PARAMS}  # every default is a guess until calibrated
SPEED = {"claude-code": 1.0, "codex": 1.3, "grok": 1.6}  # work time multipliers, guesses


def defaults() -> dict:
    return {k: d for k, _, _, _, _, _, d in PARAMS}


class Sim:
    def __init__(self, params: dict | None = None, seed: int = 1):
        self.p = {**defaults(), **(params or {})}
        self.seed = seed
        self.rng = random.Random(seed)
        self.env = simpy.Environment()
        self.w = World()
        self.lines: list[dict] = []  # every line, as the log would hold it
        self.number = 1000  # GitHub's numbers, shared by issues and pull requests
        self.kick = self.env.event()  # wakes the watchdog and merger early
        self.planned: set[int] = set()
        self.pending = False  # a config waiting for departing agents
        self._config()
        for proc in (self.human(), self.foreman(), self.watchdog(), self.initiatives()):
            self.env.process(proc)

    # ------------------------------------------------------------ the log

    @property
    def t(self) -> datetime:
        return START + timedelta(minutes=self.env.now)

    def emit(self, actor: str, **body) -> m.Line:
        t = self.t.replace(microsecond=0)
        if self.w.t and t < self.w.t:
            t = self.w.t
        line = m.Line(self.w.seq + 1, t, actor, m.parse_body(json.dumps(body)))
        self.w.apply(line)  # refuses illegal moves: the model must make none
        self.lines.append(json.loads(line.dump()))
        if not self.kick.triggered:
            self.kick.succeed()
        return line

    def run_until(self, minutes: float) -> None:
        if minutes > self.env.now:
            self.env.run(until=minutes)

    def poke(self) -> None:
        """Parameters changed: agent counts take effect in a new config."""
        want = {t: int(self.p[t]) for t in SPEED}
        have = {a.type: a.count for a in self.w.config.agents}
        self.pending = want != have or self._reviewer() != self.w.config.reviewers[0]
        if self.pending:
            self.pending = not self._config()

    def _reviewer(self) -> str:
        return "review-mux" if self.p["review_mux"] >= 1 else "placeholder"

    def _config(self) -> bool:
        """Apply a config from the parameters; False while an agent that would
        leave is still live or holds work (the watchdog tries again)."""
        c = json.loads(CONFIG.read_text())
        c["version"] = (self.w.config.version + 1) if self.w.config else 1
        keep = {a["type"]: a for a in c["agents"]}
        c["agents"] = [dict(keep[t], count=int(self.p[t])) for t in SPEED if int(self.p[t]) > 0]
        if not c["agents"]:
            c["agents"] = [dict(keep["claude-code"], count=1)]
        c["reviewers"] = [self._reviewer()]
        old = set(self.w.config.names()) if self.w.config else set()
        # Agents that leave the config must be off and hold nothing first.
        if self.w.config:
            new = {f"{a['type']}-{i}" for a in c["agents"] for i in range(1, a["count"] + 1)}
            for a in old - new:
                if not self.w.is_(M.AGENT, a, AgentS.off) or a in self.w.holding:
                    return False
        self.emit("human", event="config_applied", config=c)
        return True

    def _next(self) -> int:
        self.number += 1
        return self.number

    def _sha(self) -> str:
        return "%040x" % self.rng.getrandbits(160)

    def _wait(self, mean: float):
        return self.env.timeout(self.rng.expovariate(1 / mean) if mean > 0 else 0)

    def _lognormal(self, median: float) -> float:
        return median * self.rng.lognormvariate(0, 0.7)

    # ------------------------------------------------------------ the human

    def human(self):
        while True:
            rate = self.p["issues_per_day"]
            if rate <= 0:
                yield self.env.timeout(60)
                continue
            yield self._wait(DAY / rate)
            roll = self.rng.uniform(0, 100)
            kind = ("bug" if roll < self.p["bug_share"] else
                    "friction" if roll < self.p["bug_share"] + self.p["friction_share"] else "enhancement")
            n = self._next()
            open_work = [k for k, i in self.w.issues.items()
                         if i.kind is not m.Kind.initiative and self.w.open_(k)]
            deps = [self.rng.choice(open_work)] if open_work and self.rng.random() < 0.1 else []
            self.emit("human", event="filed", issue=n, kind=kind, deps=deps)
            if self.rng.uniform(0, 100) < self.p["hold_odds"]:
                self.env.process(self.hold(n))

    def hold(self, n: int):
        if not self.w.waiting(n):
            return
        self.emit("human", event="parked", issue=n, reason="owner")
        yield self._wait(DAY / 2)
        if self.w.open_(n) and self.w.issues[n].parked:
            self.emit("human", event="unparked", issue=n)

    def initiatives(self):
        while True:
            rate = self.p["initiatives_per_week"]
            if rate <= 0:
                yield self.env.timeout(60)
                continue
            yield self._wait(7 * DAY / rate)
            self.emit("human", event="filed", issue=self._next(), kind="initiative")

    # ------------------------------------------------------------ the foreman

    def foreman(self):
        while True:
            yield self.env.timeout(self.w.config.foreman.tick_minutes)
            w = self.w
            self.emit("foreman", event="tick_started", tick=w.tick + 1)
            for agent, issue in list(w.holding.items()):
                if w.is_(M.AGENT, agent, AgentS.off):
                    self.emit("foreman", event="returned", issue=issue, reason="stale")
                elif w.is_(M.QUOTA, agent, "out"):
                    self.emit("foreman", event="returned", issue=issue, reason="quota")
            self._preempt()
            for n, i in list(w.issues.items()):
                if (i.kind is m.Kind.initiative and w.open_(n) and i.children
                        and not any(w.open_(c) for c in i.children)):
                    self.emit("foreman", event="closed", issue=n, reason="done")
            self.emit("foreman", event="tick_ended", tick=w.tick)
            for n, i in list(w.issues.items()):
                if (i.kind is m.Kind.initiative and w.open_(n) and not i.children
                        and not i.parked and n not in self.planned):
                    self.planned.add(n)
                    self.env.process(self.plan(n))

    def _preempt(self):
        w = self.w
        limit = w.config.foreman.preempt_minutes
        if limit is None or w.free_agents():
            return
        cutoff = w.t - timedelta(minutes=limit)
        if not any(i.kind is m.Kind.bug and w.is_(M.ISSUE, n, IssueS.ready)
                   and w.since(M.ISSUE, n) <= cutoff for n, i in w.issues.items()):
            return
        held = [n for n in w.holding.values() if w.issues[n].kind is not m.Kind.bug]
        if held:
            n = max(held, key=lambda n: w.since(M.ISSUE, n))
            self.emit("foreman", event="returned", issue=n, reason="preempted",
                      note="a bug has waited longer than the preemption limit")

    def plan(self, initiative: int):
        """A planning session files the initiative's issues, in dependency order."""
        yield self.env.timeout(self._lognormal(self.p["plan_minutes"]))
        if not self.w.open_(initiative):
            return
        made = []
        for _ in range(max(1, int(self.p["children"]))):
            n = self._next()
            deps = [self.rng.choice(made)] if made and self.rng.random() < 0.4 else []
            self.emit("foreman", event="filed", issue=n, kind="enhancement",
                      parent=initiative, deps=deps)
            made.append(n)
            yield self.env.timeout(0.5)

    # ------------------------------------------------------------ the watchdog

    def watchdog(self):
        out_since: dict[str, float] = {}
        while True:
            w = self.w
            if self.pending:
                self.pending = not self._config()
            leaving = self._leaving() if self.pending else set()
            for a in w.config.names():
                if a in leaving:
                    continue  # no new session for an agent the next config drops
                if w.is_(M.QUOTA, a, "out"):
                    out_since.setdefault(a, self.env.now)
                    if self.env.now - out_since[a] >= self.p["quota_rest"]:
                        self.emit("watchdog", event="quota_back", agent=a)
                        out_since.pop(a)
                    continue
                if w.is_(M.AGENT, a, AgentS.off) and (a in w.holding or w.ready_queue(a)):
                    self.emit("watchdog", event="session_started", agent=a)
                    self.env.process(self.session(a))
            self.kick = self.env.event()
            yield self.env.any_of([self.env.timeout(1), self.kick])
            yield self.env.timeout(0.05)

    def _leaving(self) -> set[str]:
        new = {f"{t}-{i}" for t in SPEED for i in range(1, int(self.p[t]) + 1)}
        return set(self.w.config.names()) - new

    def end(self, agent: str, reason: str):
        self.emit("watchdog", event="session_ended", agent=agent, reason=reason)
        if reason == "quota":
            self.emit("watchdog", event="quota_out", agent=agent)

    # ------------------------------------------------------------ an agent's session

    def session(self, a: str):
        yield self.env.timeout(0.2)  # reading the prompt
        w = self.w
        if a in w.holding:
            n = w.holding[a]
        else:
            queue = w.ready_queue(a)
            if not queue:
                self.end(a, "finished")
                return
            n = queue[0]
            self.emit(a, event="taken", issue=n)
        kind = a.rsplit("-", 1)[0]
        minutes = self._lognormal(self.p["work_minutes"]) * SPEED[kind]
        # Quota can run out in the middle of the work.
        p_out = 1 - (1 - self.p["quota_per_hour"] / 100) ** (minutes / 60)
        if self.rng.random() < p_out:
            yield self.env.timeout(minutes * self.rng.random())
            self.end(a, "quota" if w.holding.get(a) == n else "finished")
            return
        if self.rng.uniform(0, 100) < self.p["crash"]:
            yield self.env.timeout(minutes * self.rng.random())
            self.end(a, "crashed" if w.holding.get(a) == n else "finished")
            return
        yield self.env.timeout(minutes)
        if w.holding.get(a) != n:  # taken back meanwhile (preempted, closed on GitHub)
            self.end(a, "finished")
            return
        if self.rng.uniform(0, 100) < self.p["finds_bug"]:
            self.emit(a, event="filed", issue=self._next(), kind="bug")
        roll = self.rng.uniform(0, 100)
        g, b, v = self.p["give_up"], self.p["blocked"], self.p["verified"]
        if roll < g:
            self.emit(a, event="returned", issue=n, reason="gave_up", note="cannot be done as written")
        elif roll < g + b:
            dep = self._next()
            self.emit(a, event="filed", issue=dep, kind="bug")
            self.emit(a, event="depends", issue=n, on=dep)
            self.emit(a, event="returned", issue=n, reason="blocked")
        elif roll < g + b + v and w.issues[n].pr is None:
            self.emit(a, event="closed", issue=n, reason="completed", note="checked: nothing to change")
        else:
            i = w.issues[n]
            pr = i.pr if i.pr is not None else self._next()
            push = w.prs[pr].pushes + 1 if pr in w.prs else 1
            self.emit(a, event="push", issue=n, pr=pr, push=push, sha=self._sha())
            self.env.process(self.checks(pr, push))
        self.end(a, "finished")

    # ------------------------------------------------------------ CI, review, merge

    def checks(self, pr: int, push: int):
        reviewers = list(self.w.prs[pr].reviewers)
        procs = [self.env.process(self.ci(pr, push))]
        procs += [self.env.process(self.review(pr, push, r)) for r in reviewers]
        yield self.env.all_of(procs)
        w = self.w
        p = w.prs[pr]
        if p.pushes != push or not w.is_(M.PR, pr, "open"):
            return
        if green(w, pr) and w.is_(M.ISSUE, p.issue, IssueS.checking):
            yield self.env.timeout(1)  # the observer's next pass
            if self.rng.uniform(0, 100) < self.p["conflict"]:
                self.emit("merger", event="merge_failed", pr=pr, push=push, reason="conflict")
            else:
                self.emit("merger", event="merged", pr=pr, push=push, sha=self._sha())

    def ci(self, pr: int, push: int):
        yield self.env.timeout(self._lognormal(self.p["ci_minutes"]))
        if self._current(pr, push) and self.w.is_(M.CI, f"{pr}/{push}", "pending"):
            ok = self.rng.uniform(0, 100) >= self.p["ci_fail"]
            self.emit("ci", event="ci_done", pr=pr, push=push, result="pass" if ok else "fail")

    def review(self, pr: int, push: int, r: str):
        yield self.env.timeout(0.1 if r == "placeholder" else self._lognormal(self.p["review_minutes"]))
        if not (self._current(pr, push) and self.w.is_(M.REVIEW, f"{pr}/{push}/{r}", "pending")):
            return
        if r != "placeholder" and self.rng.uniform(0, 100) < self.p["review_changes"]:
            self.emit("reviewer", event="review_done", by=r, pr=pr, push=push, result="changes",
                      reasons=["other"], note="findings from the review bots")
        else:
            self.emit("reviewer", event="review_done", by=r, pr=pr, push=push, result="approved")

    def _current(self, pr: int, push: int) -> bool:
        return self.w.prs[pr].pushes == push and self.w.is_(M.PR, pr, "open")
