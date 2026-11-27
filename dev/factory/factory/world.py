"""Which line may come next.

The world is the log folded so far: one current state per machine instance
(machines.py), the facts that are not states (dependencies, which pull request
belongs to which issue, builds), and every closed state interval. apply() takes
one line to the next world or raises Illegal and changes nothing that matters:
callers that keep going after a refusal apply to a copy (see ledger.py).
"""
from __future__ import annotations

import copy
from dataclasses import dataclass, field
from datetime import datetime, timedelta
from typing import Callable

from . import machines as M
from . import model as m
from .machines import AgentS, CiS, IssueS, ReviewS
from .model import Kind, Role


class Illegal(Exception):
    def __init__(self, seq: int, msg: str):
        super().__init__(f"line {seq}: {msg}")
        self.seq, self.msg = seq, msg


@dataclass(frozen=True)
class Interval:
    machine: str
    key: str
    state: str
    start: int  # seq that entered the state
    end: int | None  # seq that left it; None while it lasts
    start_t: datetime
    end_t: datetime | None


@dataclass
class Issue:
    kind: Kind
    parent: int | None
    deps: set[int]
    filed: datetime
    found: tuple | None  # bugs and friction: ("work", issue), ("use", use), ("report",)
    children: set[int] = field(default_factory=set)
    pr: int | None = None
    holder: str | None = None
    parked: bool = False
    given_up: set[str] = field(default_factory=set)  # agents that gave up on it
    closer: str | None = None  # who closed it, in the log
    owner: str | None = None  # the agent that held it last: work coming back waits for it
    asked: bool = False  # parked on a question its holder asked
    hold: str | None = None  # why it is parked: owner, lock, dictator; None when asked
    stuck: dict[str, int] = field(default_factory=dict)  # agent -> sessions ended without moving it


@dataclass
class Pr:
    issue: int
    pushes: int = 0
    sha: str = ""  # of the latest push
    reviewers: tuple[str, ...] = ()  # whose approval the latest push needs
    merged: datetime | None = None


@dataclass
class Version:
    frozen: datetime
    builds: list[datetime] = field(default_factory=list)
    backports: set[int] = field(default_factory=set)
    unbuilt: set[int] = field(default_factory=set)


@dataclass
class Use:
    by: str
    version: str
    build: int
    void: bool = False  # ended without running to its end: not a use of the build


HANDLERS: dict[type, Callable] = {}


def on(cls):
    def reg(fn):
        HANDLERS[cls] = fn
        return fn
    return reg


@dataclass
class World:
    seq: int = 0
    t: datetime | None = None
    config: m.Config | None = None
    scope_frozen: bool = False
    tick: int = 0
    numbers: set[int] = field(default_factory=set)  # GitHub's, shared by issues and prs
    issues: dict[int, Issue] = field(default_factory=dict)
    prs: dict[int, Pr] = field(default_factory=dict)
    holding: dict[str, int] = field(default_factory=dict)
    quota_seen: dict[str, tuple] = field(default_factory=dict)  # harness -> (when, windows)
    moved: dict[str, bool] = field(default_factory=dict)  # agent -> moved its work this session
    versions: dict[str, Version] = field(default_factory=dict)
    uses: dict[int, Use] = field(default_factory=dict)
    attribution: dict[int, int] = field(default_factory=dict)  # bug -> seq of latest
    current: dict[tuple[str, str], tuple[str, int, datetime]] = field(default_factory=dict)
    intervals: list[Interval] = field(default_factory=list)

    # ------------------------------------------------------------ machinery

    def fail(self, msg: str):
        raise Illegal(self.seq + 1, msg)

    def state(self, mc: M.Machine, key) -> str | None:
        cur = self.current.get((mc.name, str(key)))
        return cur[0] if cur else None

    def since(self, mc: M.Machine, key) -> datetime:
        return self.current[(mc.name, str(key))][2]

    def is_(self, mc: M.Machine, key, *states) -> bool:
        return self.state(mc, key) in states

    def enter(self, mc: M.Machine, key, new: str) -> None:
        k = (mc.name, str(key))
        cur = self.current.get(k)
        old = mc.states(cur[0]) if cur else None
        if not mc.allows(old, mc.states(new)):
            self.fail(f"{mc.name} {key}: no edge {old} -> {new}")
        assert self.t
        if cur:
            self.intervals.append(Interval(mc.name, k[1], cur[0], cur[1], self.seq + 1,
                                           cur[2], self.t))
        self.current[k] = (str(new), self.seq + 1, self.t)

    def moves(self) -> set[tuple[str, str | None, str]]:
        """Every (machine, from, to) taken so far; from is None at creation."""
        by_key: dict[tuple[str, str], list[Interval]] = {}
        for iv in self.intervals + self.open_intervals():
            by_key.setdefault((iv.machine, iv.key), []).append(iv)
        out = set()
        for (mc, _), ivs in by_key.items():
            ivs.sort(key=lambda iv: iv.start)
            out.add((mc, None, ivs[0].state))
            out |= {(mc, a.state, b.state) for a, b in zip(ivs, ivs[1:])}
        return out

    def open_intervals(self) -> list[Interval]:
        return [Interval(mc, key, s, seq, None, t, None)
                for (mc, key), (s, seq, t) in self.current.items()]

    # ------------------------------------------------------------ derived

    def issue(self, n: int, *, work: bool | None = None) -> Issue:
        i = self.issues.get(n)
        if i is None:
            self.fail(f"no issue {n}")
        if work is True and i.kind is Kind.initiative:
            self.fail(f"issue {n} is an initiative")
        if work is False and i.kind is not Kind.initiative:
            self.fail(f"issue {n} is not an initiative")
        return i

    def open_(self, n: int) -> bool:
        mc = M.INITIATIVE if self.issues[n].kind is Kind.initiative else M.ISSUE
        return not self.is_(mc, n, "closed")

    def waiting(self, n: int) -> bool:
        return self.is_(M.ISSUE, n, IssueS.ready, IssueS.blocked, IssueS.parked)

    def needs_work(self, n: int) -> bool:
        return self.waiting(n) and self.issues[n].pr is not None

    def agent(self, name: str) -> str:
        assert self.config
        if self.config.type_of(name) is None:
            self.fail(f"no agent {name} in config {self.config.version}")
        return name

    def free_agents(self) -> list[str]:
        assert self.config
        return [a for a in self.config.names()
                if self.is_(M.AGENT, a, AgentS.idle) and self.is_(M.QUOTA, a, "ok")]

    def latest_push(self, pr: int, push: int) -> Pr:
        p = self.prs.get(pr)
        if p is None:
            self.fail(f"no pr {pr}")
        if push != p.pushes:
            self.fail(f"push {push} is not pr {pr}'s latest ({p.pushes})")
        return p

    def quota_back_at(self, agent: str, retry: timedelta) -> datetime:
        """When to try an agent that is out of quota again, from what its
        harness last said about its plan (quota_seen):

        - a reading taken since it ran out, with no limit full: now;
        - a limit read as full: when that limit resets, however far off;
        - a reading from before it ran out: when the limit that was nearly
          full then (90%) resets;
        - nothing to go on: `retry` after it ran out.

        A limit named for one model, as "Current week (Fable)", counts only
        for agents on that model."""
        out = self.since(M.QUOTA, agent)
        t = next(a for a in self.config.agents if agent.rsplit("-", 1)[0] == a.type)
        seen = self.quota_seen.get(str(t.type))
        if seen is None:
            return out + retry
        when, windows = seen

        def mine(x) -> bool:
            only = x.name[x.name.rfind("(") + 1:-1].lower() if x.name.endswith(")") else ""
            return not only or only == "all models" or only in str(t.model).lower()
        full = [x.resets for x in windows if mine(x) and x.used >= 100 and x.resets]
        if full:  # a reset already past when it ran out says nothing: the reading is stale
            return max(full) if max(full) > out else out + retry
        if when >= out:
            return when
        near = [x.resets for x in windows if mine(x) and x.used >= 90 and x.resets and x.resets > out]
        return min(near) if near else out + retry

    def reserved_for(self, n: int) -> str | None:
        """The agent a ready issue waits for: the one that held it last, while
        it has quota and for affinity_minutes after the issue came back."""
        i = self.issues[n]
        a = i.owner
        if (a is None or not self.config or self.config.dictator.affinity_minutes is None
                or not self.is_(M.ISSUE, n, IssueS.ready) or a in i.given_up
                or self.config.type_of(a) is None or not self.is_(M.QUOTA, a, "ok")):
            return None
        if self.t - self.since(M.ISSUE, n) >= timedelta(minutes=self.config.dictator.affinity_minutes):
            return None
        return a

    def ready_queue(self, agent: str | None = None) -> list[int]:
        """Ready issues best first: its own come-back work first, then needing
        work, then bugs, then the rest, oldest first; with agent, only those
        it may take (not ones it gave up on, nor ones waiting for another
        agent). The order is policy; taking a reserved issue is refused."""
        ready = [n for n in self.issues if self.is_(M.ISSUE, n, IssueS.ready)
                 and agent not in self.issues[n].given_up
                 and (agent is None or self.reserved_for(n) in (None, agent))]
        return sorted(ready, key=lambda n: (agent is None or self.reserved_for(n) != agent,
                                            self.issues[n].pr is None,
                                            self.issues[n].kind is not Kind.bug,
                                            self.issues[n].filed, n))

    def agent_view(self, name: str) -> dict:
        """What an agent needs to know about itself."""
        n = self.holding.get(name)
        out: dict = {"agent": name, "state": self.state(M.AGENT, name),
                     "quota": self.state(M.QUOTA, name), "issue": n}
        if n is not None:
            i = self.issues[n]
            pr = self.prs.get(i.pr) if i.pr else None
            out |= {"kind": str(i.kind), "parent": i.parent, "pr": i.pr,
                    "next_push": (pr.pushes if pr else 0) + 1}
            if pr and pr.pushes:
                key = f"{i.pr}/{pr.pushes}"
                out |= {"last_push": {"sha": pr.sha, "ci": self.state(M.CI, key),
                                      "reviews": {r: self.state(M.REVIEW, f"{key}/{r}")
                                                  for r in pr.reviewers}}}
        return out

    def status(self) -> str:
        """The factory at a glance, for a person."""
        assert self.config and self.t
        def ago(t):
            mins = int((self.t - t).total_seconds() // 60)
            return f"{mins // 60}h{mins % 60:02d}m" if mins >= 60 else f"{mins}m"
        out = [f"log line {self.seq}, {self.t:%Y-%m-%d %H:%M}Z, config {self.config.version}", "",
               "agents"]
        for a in self.config.names():
            what = self.state(M.AGENT, a)
            if a in self.holding:
                what += f" on #{self.holding[a]}"
            if self.is_(M.QUOTA, a, "out"):
                what += f", out of quota {ago(self.since(M.QUOTA, a))}"
            out.append(f"  {a:16} {what}")
        out += ["", "issues"]
        for st in ("ready", "blocked", "parked", "held", "checking"):
            ns = [n for n in self.issues if self.is_(M.ISSUE, n, st)]
            if ns:
                out.append(f"  {st:9} " + " ".join(
                    f"#{n}" + ("*" if self.issues[n].kind is Kind.bug else "")
                    + (f"(pr {self.issues[n].pr})" if self.issues[n].pr else "")
                    for n in sorted(ns)))
        inits = [n for n, i in self.issues.items() if i.kind is Kind.initiative and self.open_(n)]
        if inits:
            out.append("  initiatives " + " ".join(
                f"#{n}" + (" (parked)" if self.issues[n].parked else
                           f" ({sum(self.open_(c) for c in self.issues[n].children)}"
                           f"/{len(self.issues[n].children)} open)")
                for n in sorted(inits)))
        names = set(self.config.names()) if self.config else set()
        gave = [f"#{n}" for n, i in self.issues.items() if self.waiting(n) and names and names <= i.given_up]
        if gave:
            out.append("  every agent gave up on  " + " ".join(gave))
        asked = [f"#{n}" for n, i in self.issues.items() if i.asked]
        if asked:
            out.append("  asked, waiting for an answer  " + " ".join(asked))
        closed = [n for n in self.issues if self.is_(M.ISSUE, n, "closed")]
        out.append(f"  closed    {len(closed)}   (* bug)")
        checking = [n for n in self.issues if self.is_(M.ISSUE, n, "checking")]
        if checking:
            out += ["", "in checks"]
            for n in sorted(checking):
                pr = self.prs[self.issues[n].pr]
                key = f"{self.issues[n].pr}/{pr.pushes}"
                revs = ", ".join(f"{r} {self.state(M.REVIEW, f'{key}/{r}')}" for r in pr.reviewers)
                out.append(f"  #{n} pr {self.issues[n].pr} push {pr.pushes}: "
                           f"ci {self.state(M.CI, key)}, {revs}")
        return "\n".join(out)

    def hold_reasons(self, version: str) -> set[m.HoldReason]:
        assert self.config and self.t
        v, rules, R = self.versions[version], self.config.release, m.HoldReason
        build = len(v.builds)
        out = set()
        bugs = [n for n, i in self.issues.items() if i.kind is Kind.bug]
        if any(self.open_(n) for n in bugs):
            out.add(R.known_bugs)
        uses = sum(1 for n, u in self.uses.items()
                   if (u.version, u.build, u.by) == (version, build, Role.power_user)
                   and self.is_(M.USE, n, "ended") and not u.void)
        if uses < rules.uses_required:
            out.add(R.uses)
        found_here = [self.issues[n].filed for n in bugs
                      if (f := self.issues[n].found) and f[0] == "use"
                      and self.uses[f[1]].version == version]
        if self.t - max([v.builds[-1], *found_here]) < timedelta(minutes=rules.soak_minutes):
            out.add(R.soak)
        if v.unbuilt:
            out.add(R.stale_build)
        return out

    # ------------------------------------------------------------ apply

    def attempt(self, line: m.Line) -> World:
        """The next world, leaving this one as it was whether or not the line
        is legal. apply() is the fast path for a reader that stops at the
        first illegal line."""
        nxt = copy.deepcopy(self)
        nxt.apply(line)
        return nxt

    def apply(self, line: m.Line) -> None:
        e, a = line.body, line.actor
        if line.seq != self.seq + 1:
            self.fail(f"seq {line.seq} after {self.seq}")
        if self.t and line.t < self.t:
            self.fail("t is before the line above")
        if self.config is None and not isinstance(e, m.ConfigApplied):
            self.fail("the first line is config_applied")
        is_agent = bool(self.config and self.config.type_of(a))
        if a not in m.ROLES and not is_agent:
            self.fail(f"{a!r} is no role and no agent in the config")
        if not (a in e.actors or (is_agent and m.AGENT in e.actors)):
            self.fail(f"{a} may not write {e.event}")
        if is_agent and self.is_(M.AGENT, a, AgentS.off):
            self.fail(f"{a} is off; an agent writes only from a live session")
        if a == Role.dictator and e.nature is m.Nature.decision \
                and not isinstance(e, m.TickStarted) and not self.is_(M.DICTATOR, "", "ticking"):
            self.fail("the dictator decides only inside a tick")
        self.t = line.t
        held = self.holding.get(a) if is_agent else None
        HANDLERS[type(e)](self, e, a)
        if is_agent and isinstance(e, MOVES):  # progress on its work: not stuck
            self.moved[a] = True
            for n in {held, getattr(e, "issue", None)} - {None}:
                self.issues[n].stuck.pop(a, None)
        self._settle()
        self.seq = line.seq

    def _waiting_state(self, n: int) -> IssueS:
        i = self.issues[n]
        if i.parked:
            return IssueS.parked
        return IssueS.blocked if any(self.open_(d) for d in i.deps) else IssueS.ready

    def wait(self, n: int) -> None:
        """Back to waiting: blocked exactly while a dependency is open."""
        self.enter(M.ISSUE, n, self._waiting_state(n))

    def _settle(self) -> None:
        for n in self.issues:
            if self.waiting(n) and self.state(M.ISSUE, n) != self._waiting_state(n):
                self.wait(n)


# ---------------------------------------------------------------- handlers

# What an agent writes that moves its work (taking alone does not).
MOVES = (m.Push, m.Returned, m.Closed, m.Depends, m.Undepends, m.Filed)


@on(m.ConfigApplied)
def _(w: World, e: m.ConfigApplied, a):
    if w.config and e.config.version != w.config.version + 1:
        w.fail(f"config version {e.config.version} after {w.config.version}")
    if not w.config and e.config.version != 1:
        w.fail("the first config is version 1")
    if a == Role.dictator:
        _dictator_may(w, e.config)
    new = set(e.config.names())
    for name in (w.config.names() if w.config else []):
        if name not in new and (not w.is_(M.AGENT, name, AgentS.off) or name in w.holding):
            w.fail(f"config drops {name}, which is live or holds an issue")
    w.config = e.config
    for name in new:
        if w.state(M.AGENT, name) is None:
            w.enter(M.AGENT, name, AgentS.off)
            w.enter(M.QUOTA, name, "ok")
    if w.state(M.DICTATOR, "") is None:
        w.enter(M.DICTATOR, "", "resting")


DICTATOR_TUNES = ("preempt_minutes", "affinity_minutes", "no_progress_sessions",
                 "session_minutes", "orchestrate_minutes")
MAX_AGENTS = 4  # of one type, when the dictator sets the count


def _dictator_may(w: World, new: m.Config) -> None:
    """The dictator changes only how many agents of each type run, and its own
    timing rules. Who reviews, on which plans and models, and how releases
    go stay the human's."""
    if w.config is None:
        w.fail("the first config is the human's")
    old = w.config.model_dump(mode="json")
    want = new.model_dump(mode="json")
    for d in (old, want):
        d.pop("version")
        d.pop("guesses")
        for k in DICTATOR_TUNES:
            d["dictator"].pop(k)
    counts = [x.pop("count") for x in want["agents"]]
    for x in old["agents"]:
        x.pop("count")
    if old != want:
        w.fail("the dictator changes only agent counts and its own timing rules")
    if any(c > MAX_AGENTS for c in counts):
        w.fail(f"the dictator runs at most {MAX_AGENTS} agents of a type")


@on(m.ScopeFrozen)
def _(w: World, e, a):
    if w.scope_frozen:
        w.fail("scope is already frozen")
    w.scope_frozen = True


@on(m.Filed)
def _(w: World, e: m.Filed, a):
    if e.issue in w.numbers:
        w.fail(f"#{e.issue} is taken")
    held = w.holding.get(a)
    found = None
    if e.kind in (Kind.bug, Kind.friction):
        if held is not None:
            found = ("work", held)
        elif a in (Role.human, Role.power_user):
            use = next((n for n, u in w.uses.items()
                        if u.by == a and w.is_(M.USE, n, "open")), None)
            if use is not None:
                found = ("use", use)
            elif a == Role.human:
                found = ("report",)  # the human is outside: it reports whenever
            else:
                w.fail("the power user files bugs and friction from an open use session")
        else:
            w.fail(f"{a} does not find {e.kind}s")
    else:
        if w.scope_frozen:
            w.fail(f"scope is frozen; no new {e.kind}")
        planners = {Role.human, Role.dictator} | ({m.AGENT} if e.kind is Kind.enhancement else set())
        if (a if a in m.ROLES else m.AGENT) not in planners:
            w.fail(f"{a} does not file {e.kind}s")
        if a not in m.ROLES:
            if held is None:
                w.fail("an agent files follow-up work only from the issue it holds")
            if e.parent != w.issues[held].parent:
                w.fail(f"follow-up work goes under #{held}'s initiative")
    if e.parent is not None:
        w.issue(e.parent, work=False)
        if not w.open_(e.parent):
            w.fail(f"initiative #{e.parent} is closed")
    for d in e.deps:
        w.issue(d, work=True)
        if not w.open_(d):
            w.fail(f"dependency #{d} is closed")
    w.numbers.add(e.issue)
    w.issues[e.issue] = Issue(e.kind, e.parent, set(e.deps), w.t, found)
    if e.parent is not None:
        w.issues[e.parent].children.add(e.issue)
    if e.kind is Kind.initiative:
        w.enter(M.INITIATIVE, e.issue, "open")
    else:
        w.wait(e.issue)


@on(m.Depends)
def _(w: World, e: m.Depends, a):
    i = w.issue(e.issue, work=True)
    w.issue(e.on, work=True)
    if not (w.open_(e.issue) and w.open_(e.on)):
        w.fail("both issues must be open")
    if e.on in i.deps:
        w.fail(f"#{e.issue} already depends on #{e.on}")
    stack, seen = [e.on], set()
    while stack:
        n = stack.pop()
        if n == e.issue:
            w.fail(f"#{e.issue} -> #{e.on} makes a cycle")
        if n not in seen:
            seen.add(n)
            stack.extend(w.issues[n].deps)
    i.deps.add(e.on)


@on(m.Parked)
def _(w: World, e: m.Parked, a):
    whose = {m.ParkReason.owner: Role.human, m.ParkReason.lock: Role.outside,
             m.ParkReason.dictator: Role.dictator}[e.reason]
    if a != whose:
        w.fail("the owner's hold is the human's; a lock is an outsider's; the dictator's is its own")
    i = w.issue(e.issue)
    if not w.open_(e.issue):
        w.fail(f"#{e.issue} is closed")
    if i.parked:
        w.fail(f"#{e.issue} is already parked")
    i.parked, i.hold = True, e.reason.value


@on(m.Unparked)
def _(w: World, e: m.Unparked, a):
    i = w.issue(e.issue)
    if not i.parked:
        w.fail(f"#{e.issue} is not parked")
    if a == Role.dictator and not (i.asked or i.hold == m.ParkReason.dictator):
        w.fail(f"#{e.issue} is held by {i.hold}; the dictator lifts only its own hold or answers a question")
    i.parked = i.asked = False
    i.hold = None
    i.given_up.clear()  # the owner looked at it: everyone may try again


@on(m.Undepends)
def _(w: World, e: m.Undepends, a):
    i = w.issue(e.issue)
    if e.on not in i.deps:
        w.fail(f"#{e.issue} does not depend on #{e.on}")
    i.deps.discard(e.on)


def _let_go(w: World, i: Issue) -> None:
    """Something outside the factory ended the holder's claim on i."""
    holder = i.holder
    if holder is None:
        return
    if w.is_(M.AGENT, holder, AgentS.working):
        w.enter(M.AGENT, holder, AgentS.idle)
    del w.holding[holder]
    i.holder = None


@on(m.Closed)
def _(w: World, e: m.Closed, a):
    i = w.issue(e.issue)
    inside = a == Role.dictator
    if a not in m.ROLES:  # an agent
        if i.holder != a:
            w.fail(f"{a} does not hold #{e.issue}")
        if e.reason is not m.CloseReason.completed or not e.note:
            w.fail("an agent closes only as completed, with its evidence in the note")
        if i.pr is not None and w.is_(M.PR, i.pr, "open"):
            w.fail(f"#{e.issue} has open pull request {i.pr}; it closes by merging")
    if e.duplicate_of is not None:
        w.issue(e.duplicate_of)
    if e.reason is m.CloseReason.completed and inside:
        w.fail("only the human or an outsider closes work as completed")
    if i.kind is Kind.initiative:
        if inside and any(w.open_(c) for c in i.children):
            w.fail(f"initiative #{e.issue} has open children")
        w.enter(M.INITIATIVE, e.issue, "closed")
        return
    if e.reason is m.CloseReason.done:
        w.fail("work closes as done only by merging")
    if inside and not w.waiting(e.issue):
        w.fail(f"#{e.issue} is {w.state(M.ISSUE, e.issue)}; the dictator closes only waiting work")
    _let_go(w, i)
    i.closer = a
    w.enter(M.ISSUE, e.issue, IssueS.closed)
    if i.pr is not None and w.is_(M.PR, i.pr, "open"):
        w.enter(M.PR, i.pr, "abandoned")


@on(m.SessionStarted)
def _(w: World, e: m.SessionStarted, a):
    n = w.agent(e.agent)
    if not w.is_(M.QUOTA, n, "ok"):
        w.fail(f"{n} is out of quota")
    w.enter(M.AGENT, n, AgentS.working if n in w.holding else AgentS.idle)
    w.moved[n] = False


@on(m.SessionEnded)
def _(w: World, e: m.SessionEnded, a):
    n = w.agent(e.agent)
    held = w.holding.get(n)
    if held is not None and not w.moved.get(n, False):
        st = w.issues[held].stuck
        st[n] = st.get(n, 0) + 1
    w.enter(M.AGENT, n, AgentS.off)


@on(m.QuotaOut)
def _(w: World, e: m.QuotaOut, a):
    w.enter(M.QUOTA, w.agent(e.agent), "out")


@on(m.QuotaSeen)
def _(w: World, e: m.QuotaSeen, a):
    w.quota_seen[e.harness.value] = (w.t, e.windows)


@on(m.QuotaBack)
def _(w: World, e: m.QuotaBack, a):
    w.enter(M.QUOTA, w.agent(e.agent), "ok")


@on(m.Taken)
def _(w: World, e: m.Taken, a):
    if a not in w.free_agents():
        w.fail(f"{a} is not idle with quota")
    w.issue(e.issue, work=True)
    if not w.is_(M.ISSUE, e.issue, IssueS.ready):
        w.fail(f"#{e.issue} is {w.state(M.ISSUE, e.issue)}, not ready")
    if a in w.issues[e.issue].given_up:
        w.fail(f"{a} gave up on #{e.issue}; the owner holding and releasing it lets it try again")
    r = w.reserved_for(e.issue)
    if r not in (None, a):
        w.fail(f"#{e.issue} waits for {r}, which held it last")
    w.issues[e.issue].owner = a
    w.enter(M.ISSUE, e.issue, IssueS.held)
    w.enter(M.AGENT, a, AgentS.working)
    w.holding[a] = e.issue
    w.issues[e.issue].holder = a


@on(m.Returned)
def _(w: World, e: m.Returned, a):
    i, R = w.issue(e.issue, work=True), m.ReturnReason
    holder = i.holder
    if holder is None:
        w.fail(f"#{e.issue} is not held")
    if e.reason in (R.blocked, R.gave_up):
        if a != holder:
            w.fail(f"only {holder} returns #{e.issue} as {e.reason}")
        if e.reason is R.blocked and not any(w.open_(d) for d in i.deps):
            w.fail(f"#{e.issue} has no open dependency")
        if e.reason is R.gave_up:
            i.given_up.add(holder)
    elif e.reason is R.asked:
        if a != holder:
            w.fail(f"only {holder} returns #{e.issue} as asked")
        if not e.note:
            w.fail("an agent that asks says what in the note")
        i.parked = i.asked = True
    elif e.reason is R.no_progress:
        if a != Role.dictator:
            w.fail("only the dictator returns an issue as no_progress")
        assert w.config
        limit = w.config.dictator.no_progress_sessions
        if limit is None or i.stuck.get(holder, 0) < limit:
            w.fail(f"{holder} has ended {i.stuck.get(holder, 0)} sessions on #{e.issue} without moving it")
        if not w.is_(M.AGENT, holder, AgentS.off):
            w.fail(f"{holder}'s session is live")
        i.given_up.add(holder)
    else:
        if a != Role.dictator:
            w.fail(f"only the dictator returns an issue as {e.reason}")
        if e.reason is R.quota and not w.is_(M.QUOTA, holder, "out"):
            w.fail(f"{holder} has quota")
        if e.reason is R.stale and not w.is_(M.AGENT, holder, AgentS.off):
            w.fail(f"{holder}'s session is live")
        if e.reason is R.preempted:
            _check_preempt(w, e.issue)
    if e.reason is R.no_progress:
        i.stuck.pop(holder, None)
    w.wait(e.issue)
    if w.is_(M.AGENT, holder, AgentS.working):
        w.enter(M.AGENT, holder, AgentS.idle)
    del w.holding[holder]
    i.holder = None


def _check_preempt(w: World, n: int) -> None:
    assert w.config and w.t
    limit = w.config.dictator.preempt_minutes
    if limit is None:
        w.fail("preemption is off in this config")
    if w.issues[n].kind is Kind.bug:
        w.fail("a bug is never preempted")
    if w.free_agents():
        w.fail(f"{w.free_agents()[0]} is free")
    cutoff = w.t - timedelta(minutes=limit)
    if not any(i.kind is Kind.bug and w.is_(M.ISSUE, b, IssueS.ready)
               and w.since(M.ISSUE, b) <= cutoff
               for b, i in w.issues.items()):
        w.fail(f"no bug has been ready {limit} minutes")


@on(m.Push)
def _(w: World, e: m.Push, a):
    i = w.issue(e.issue, work=True)
    if i.holder != a or not w.is_(M.AGENT, a, AgentS.working):
        w.fail(f"{a} is not working on #{e.issue}")
    if i.pr is None:
        if e.pr in w.numbers:
            w.fail(f"#{e.pr} is taken")
        w.numbers.add(e.pr)
        w.prs[e.pr] = Pr(e.issue)
        w.enter(M.PR, e.pr, "open")
        i.pr = e.pr
    elif e.pr != i.pr:
        w.fail(f"#{e.issue}'s pull request is #{i.pr}")
    p = w.prs[e.pr]
    if e.push != p.pushes + 1:
        w.fail(f"push {e.push} after {p.pushes}")
    if p.pushes:
        _void(w, e.pr, p.pushes, ci=True)
    assert w.config
    p.pushes, p.sha, p.reviewers = e.push, e.sha, w.config.reviewers
    w.enter(M.CI, f"{e.pr}/{e.push}", CiS.pending)
    for r in p.reviewers:
        w.enter(M.REVIEW, f"{e.pr}/{e.push}/{r}", ReviewS.pending)
    w.enter(M.ISSUE, e.issue, IssueS.checking)
    w.enter(M.AGENT, a, AgentS.idle)
    del w.holding[a]
    i.holder = None


def _void(w: World, pr: int, push: int, ci: bool) -> None:
    """End the push's checks that are still pending, unfinished."""
    keys = [f"{pr}/{push}/{r}" for r in w.prs[pr].reviewers]
    for mc, key in [(M.CI, f"{pr}/{push}")] * ci + [(M.REVIEW, k) for k in keys]:
        if w.is_(mc, key, "pending"):
            w.enter(mc, key, "void")


@on(m.CiDone)
def _(w: World, e: m.CiDone, a):
    p = w.latest_push(e.pr, e.push)
    w.enter(M.CI, f"{e.pr}/{e.push}", e.result.value)
    if e.result is m.CiResult.failed:
        _void(w, e.pr, e.push, ci=False)
        if w.is_(M.ISSUE, p.issue, IssueS.checking):
            w.wait(p.issue)


@on(m.ReviewDone)
def _(w: World, e: m.ReviewDone, a):
    p = w.latest_push(e.pr, e.push)
    if e.by not in p.reviewers:
        w.fail(f"{e.by} is not a reviewer of pr {e.pr} push {e.push}")
    w.enter(M.REVIEW, f"{e.pr}/{e.push}/{e.by}", e.result.value)
    if e.result is m.ReviewResult.changes and w.is_(M.ISSUE, p.issue, IssueS.checking):
        w.wait(p.issue)


def green(w: World, pr: int) -> bool:
    """The latest push passed CI and every reviewer approved it."""
    p = w.prs[pr]
    return w.is_(M.CI, f"{pr}/{p.pushes}", CiS.passed) and all(
        w.is_(M.REVIEW, f"{pr}/{p.pushes}/{r}", ReviewS.approved) for r in p.reviewers)


def _green(w: World, e) -> Pr:
    p = w.latest_push(e.pr, e.push)
    if not green(w, e.pr):
        w.fail(f"pr {e.pr} push {e.push} is not green and approved")
    if not w.is_(M.ISSUE, p.issue, IssueS.checking):
        w.fail(f"#{p.issue} is not in checks")
    return p


@on(m.Diverged)
def _(w: World, e: m.Diverged, a):
    p = w.latest_push(e.pr, e.push)
    if not w.is_(M.PR, e.pr, "open"):
        w.fail(f"pr {e.pr} is {w.state(M.PR, e.pr)}")
    i = w.issues[p.issue]
    if e.reason is m.DivergeReason.foreign_push and not w.is_(M.ISSUE, p.issue, IssueS.checking):
        w.fail(f"#{p.issue} is not in checks; a foreign push matters only there")
    _void(w, e.pr, e.push, ci=True)
    if e.reason is m.DivergeReason.closed:
        w.enter(M.PR, e.pr, "abandoned")
        i.pr = None
    if w.is_(M.ISSUE, p.issue, IssueS.checking):
        w.wait(p.issue)


@on(m.MergeFailed)
def _(w: World, e: m.MergeFailed, a):
    p = w.latest_push(e.pr, e.push)
    if e.reason is m.MergeFailure.conflict and w.is_(M.CI, f"{e.pr}/{e.push}", CiS.pending):
        # GitHub starts no CI run on a pull request that conflicts with its
        # base, so the push can never go green: it needs work now.
        if not w.is_(M.ISSUE, p.issue, IssueS.checking):
            w.fail(f"#{p.issue} is not in checks")
        _void(w, e.pr, e.push, ci=True)
        w.wait(p.issue)
        return
    w.wait(_green(w, e).issue)


@on(m.Merged)
def _(w: World, e: m.Merged, a):
    if a == Role.merger:
        p = _green(w, e)
    else:  # merged on GitHub by someone outside the factory
        p = w.latest_push(e.pr, e.push)
        if not w.is_(M.PR, e.pr, "open"):
            w.fail(f"pr {e.pr} is {w.state(M.PR, e.pr)}")
        _let_go(w, w.issues[p.issue])
        _void(w, e.pr, e.push, ci=True)
    p.merged = w.t
    w.enter(M.PR, e.pr, "merged")
    if not w.is_(M.ISSUE, p.issue, IssueS.closed):
        w.enter(M.ISSUE, p.issue, IssueS.closed)


@on(m.Attributed)
def _(w: World, e: m.Attributed, a):
    if w.issue(e.bug).kind is not Kind.bug:
        w.fail(f"#{e.bug} is not a bug")
    p = w.prs.get(e.pr)
    if p is None or p.merged is None:
        w.fail(f"#{e.pr} is not merged")
    if p.issue == e.bug:
        w.fail("a bug is not introduced by its own fix")
    if p.merged > w.issues[e.bug].filed:
        w.fail(f"#{e.pr} merged after #{e.bug} was found")
    prev = w.attribution.get(e.bug)
    if e.replaces != prev:
        w.fail(f"replaces must be {prev}, the bug's latest attribution")
    w.attribution[e.bug] = w.seq + 1


@on(m.TickStarted)
def _(w: World, e: m.TickStarted, a):
    if e.tick != w.tick + 1:
        w.fail(f"tick {e.tick} after {w.tick}")
    w.enter(M.DICTATOR, "", "ticking")
    w.tick = e.tick


@on(m.TickEnded)
def _(w: World, e: m.TickEnded, a):
    if e.tick != w.tick:
        w.fail(f"tick {w.tick} is open, not {e.tick}")
    w.enter(M.DICTATOR, "", "resting")


def _candidate(w: World, v: str) -> Version:
    if v not in w.versions:
        w.fail(f"version {v} is not frozen")
    if not w.is_(M.VERSION, v, "candidate"):
        w.fail(f"version {v} is published")
    return w.versions[v]


def _latest_build(w: World, e) -> Version:
    r = _candidate(w, e.version)
    if e.build != len(r.builds):
        w.fail(f"build {e.build} is not {e.version}'s latest ({len(r.builds)})")
    return r


@on(m.Frozen)
def _(w: World, e: m.Frozen, a):
    if any(m.version_key(v) >= m.version_key(e.version) for v in w.versions):
        w.fail(f"{e.version} is not above every earlier version")
    w.versions[e.version] = Version(w.t)
    w.enter(M.VERSION, e.version, "candidate")


@on(m.Built)
def _(w: World, e: m.Built, a):
    r = _candidate(w, e.version)
    if e.build != len(r.builds) + 1:
        w.fail(f"build {e.build} after {len(r.builds)}")
    r.builds.append(w.t)
    r.unbuilt.clear()


@on(m.Backported)
def _(w: World, e: m.Backported, a):
    r = _candidate(w, e.version)
    p = w.prs.get(e.pr)
    if p is None or p.merged is None:
        w.fail(f"#{e.pr} is not merged")
    if w.issues[p.issue].kind is not Kind.bug:
        w.fail(f"#{e.pr} fixes no bug; only bug fixes enter a release branch")
    if p.merged < r.frozen:
        w.fail(f"#{e.pr} merged before {e.version} was cut, so it is in it")
    if e.pr in r.backports:
        w.fail(f"#{e.pr} is already in {e.version}")
    r.backports.add(e.pr)
    r.unbuilt.add(e.pr)


@on(m.UseStarted)
def _(w: World, e: m.UseStarted, a):
    if e.use in w.uses:
        w.fail(f"use {e.use} exists")
    if any(u.by == a and w.is_(M.USE, n, "open") for n, u in w.uses.items()):
        w.fail(f"{a} is already in a use session")
    r = w.versions.get(e.version)
    if r is None:
        w.fail(f"version {e.version} is not frozen")
    if e.build != len(r.builds):
        w.fail(f"build {e.build} is not {e.version}'s latest ({len(r.builds)})")
    published = w.is_(M.VERSION, e.version, "published")
    if a == Role.power_user and published:
        w.fail("the power user tests candidates")
    if a == Role.human and not published:
        w.fail("the human uses releases")
    w.uses[e.use] = Use(a, e.version, e.build)
    w.enter(M.USE, e.use, "open")


@on(m.UseEnded)
def _(w: World, e: m.UseEnded, a):
    u = w.uses.get(e.use)
    if u is None or u.by != a:
        w.fail(f"use {e.use} is not {a}'s")
    w.enter(M.USE, e.use, "ended")
    u.void = e.void


@on(m.ReleaseHeld)
def _(w: World, e: m.ReleaseHeld, a):
    _latest_build(w, e)
    true = w.hold_reasons(e.version)
    if set(e.reasons) != true:
        w.fail(f"the hold reasons are {sorted(true) or 'none'}, not {sorted(e.reasons)}")


@on(m.Published)
def _(w: World, e: m.Published, a):
    _latest_build(w, e)
    if held := w.hold_reasons(e.version):
        w.fail(f"{e.version} build {e.build} is held: {sorted(held)}")
    w.enter(M.VERSION, e.version, "published")


assert set(HANDLERS) == set(m.EVENTS), set(m.EVENTS) ^ set(HANDLERS)
