"""What the page draws, from a factory World and its log lines, whatever wrote
them: the real factory or the simulation. Both sources go through here."""
from __future__ import annotations

import statistics
from datetime import datetime, timedelta

from factory import machines as M
from factory.machines import AgentS, IssueS
from factory.model import Kind
from factory.world import World

COLUMNS = ["parked", "blocked", "ready", "held", "checking", "closed"]


def iso(t: datetime | None) -> str | None:
    return t.strftime("%Y-%m-%dT%H:%M:%SZ") if t else None


def mins(a: datetime, b: datetime) -> float:
    return (b - a).total_seconds() / 60


class History:
    """Hourly samples, taken as lines are applied, so a replay and the
    simulation draw the same charts."""
    KEYS = ["ready", "held", "checking", "blocked", "parked", "merged", "closed", "quota_out", "working"]

    def __init__(self):
        self.rows: list[dict] = []
        self.merged = 0
        self.closed = 0
        self._hour: datetime | None = None

    def saw(self, line: dict) -> None:
        if line["event"] == "merged":
            self.merged += 1

    def sample(self, w: World) -> None:
        if w.t is None:
            return
        hour = w.t.replace(minute=0, second=0)
        if self._hour is not None and hour <= self._hour:
            return
        self._hour = hour
        count = {c: 0 for c in COLUMNS}
        for n, i in w.issues.items():
            if i.kind is Kind.initiative:
                continue
            s = w.state(M.ISSUE, n)
            if s in count:
                count[s] += 1
        names = w.config.names() if w.config else []
        self.rows.append(dict(
            t=iso(hour), ready=count["ready"], held=count["held"], checking=count["checking"],
            blocked=count["blocked"], parked=count["parked"], merged=self.merged, closed=count["closed"],
            quota_out=sum(w.is_(M.QUOTA, a, "out") for a in names),
            working=sum(w.is_(M.AGENT, a, AgentS.working) for a in names)))


def snapshot(w: World, lines: list[dict]) -> dict:
    """Everything the page shows at one moment."""
    now = w.t
    names = w.config.names() if w.config else []
    agents = []
    for a in names:
        st = w.state(M.AGENT, a) or "off"
        agents.append(dict(
            name=a, type=a.rsplit("-", 1)[0], state=st,
            quota=w.state(M.QUOTA, a) or "ok",
            since=round(mins(w.since(M.AGENT, a), now)) if now and w.state(M.AGENT, a) else None,
            holding=w.holding.get(a)))
    issues, cols = {}, {c: [] for c in COLUMNS}
    for n, i in w.issues.items():
        s = w.state(M.ISSUE, n) if i.kind is not Kind.initiative else w.state(M.INITIATIVE, n)
        d = dict(n=n, kind=str(i.kind), state=s, holder=i.holder, pr=i.pr, parent=i.parent,
                 deps=sorted(i.deps), parked=i.parked, rework=w.needs_work(n) if s == "ready" else False,
                 age=round(mins(w.since(M.ISSUE, n), now)) if now and i.kind is not Kind.initiative else None)
        issues[n] = d
        if i.kind is not Kind.initiative and s in cols:
            cols[s].append(n)
    order = {n: k for k, n in enumerate(w.ready_queue())}
    cols["ready"].sort(key=lambda n: order.get(n, 1e9))
    cols["closed"].sort(key=lambda n: w.since(M.ISSUE, n), reverse=True)
    prs = []
    for pr, p in w.prs.items():
        if not w.is_(M.PR, pr, "open") or p.pushes == 0:
            continue
        key = f"{pr}/{p.pushes}"
        prs.append(dict(pr=pr, issue=p.issue, push=p.pushes, ci=w.state(M.CI, key),
                        reviews={r: w.state(M.REVIEW, f"{key}/{r}") for r in p.reviewers},
                        issue_state=w.state(M.ISSUE, p.issue)))
    inits = []
    for n, i in w.issues.items():
        if i.kind is Kind.initiative:
            inits.append(dict(n=n, state=w.state(M.INITIATIVE, n), parked=i.parked,
                              children=sorted(i.children),
                              done=sum(not w.open_(c) for c in i.children)))
    inits.sort(key=lambda d: (d["state"] != "open", -d["n"]))
    models = {str(t.type): (t.model, t.plan) for t in w.config.agents} if w.config else {}
    ready = bool(w.ready_queue()) if w.config else False
    for d in agents:
        d["wake"] = wake(w, d["name"], ready)
        d["model"], d["plan"] = models.get(d["type"], ("", ""))
        d["pushes"] = sum(1 for l in lines if l["event"] == "push" and l["actor"] == d["name"])
        d["gave_up"] = sum(1 for l in lines if l["event"] == "returned" and l["actor"] == d["name"]
                           and l.get("reason") == "gave_up")
        d["closed"] = sum(1 for l in lines if l["event"] == "closed" and l["actor"] == d["name"])
    for g in inits:
        g["graph"] = [dict(id=c, deps=sorted(d for d in w.issues[c].deps if d in w.issues[g["n"]].children),
                           state=issues[c]["state"]) for c in g["children"] if c in issues]
    return dict(t=iso(now), seq=w.seq, foreman=foreman(w, lines), release=release(lines), tick=w.tick, config=w.config.version if w.config else None,
                reviewers=list(w.config.reviewers) if w.config else [],
                agents=agents, issues=issues, columns=cols, prs=sorted(prs, key=lambda d: d["pr"]),
                initiatives=inits[:12], totals=totals(w, lines),
                events=[l for l in lines[-400:] if not l["event"].startswith("tick_")][-60:]
                + [l for l in lines[-3:] if l["event"] == "tick_ended"])


def totals(w: World, lines: list[dict]) -> list[list]:
    """Label, value, unit: what the factory has done, and how fast."""
    ev = [l["event"] for l in lines]
    pushes = ev.count("push")
    ci = [l for l in lines if l["event"] == "ci_done"]
    rv = [l for l in lines if l["event"] == "review_done"]
    lead, wait = [], []
    for iv in w.intervals:
        if iv.machine != M.ISSUE.name:
            continue
        if iv.state == "ready" and iv.end_t:
            wait.append(mins(iv.start_t, iv.end_t))
    for n, i in w.issues.items():
        if i.kind is not Kind.initiative and w.is_(M.ISSUE, n, IssueS.closed):
            lead.append(mins(i.filed, w.since(M.ISSUE, n)))
    work = {}
    for iv in w.intervals:
        if iv.machine == M.AGENT.name and iv.state == "working":
            end = iv.end_t or w.t
            work[iv.key] = work.get(iv.key, 0) + mins(iv.start_t, end)
    span = mins(lines[0]["t"] and datetime.fromisoformat(lines[0]["t"].replace("Z", "+00:00")), w.t) if lines and w.t else 0
    n_agents = len(w.config.names()) if w.config else 1
    med = lambda xs: round(statistics.median(xs)) if xs else None
    return [
        ["issues filed", sum(1 for i in w.issues.values() if i.kind is not Kind.initiative), ""],
        ["issues closed", sum(1 for n, i in w.issues.items() if i.kind is not Kind.initiative and not w.open_(n)), ""],
        ["pull requests merged", ev.count("merged"), ""],
        ["pushes", pushes, ""],
        ["CI failures", round(100 * sum(l["result"] == "fail" for l in ci) / len(ci)) if ci else None, "%"],
        ["reviews asking for changes", round(100 * sum(l["result"] == "changes" for l in rv) / len(rv)) if rv else None, "%"],
        ["given up", sum(1 for l in lines if l["event"] == "returned" and l.get("reason") == "gave_up"), ""],
        ["median lead time (filed to closed)", med(lead), "min"],
        ["median wait in ready", med(wait), "min"],
        ["agent time working", round(100 * sum(work.values()) / (span * n_agents)) if span else None, "%"],
    ]


def foreman(w: World, lines: list[dict]) -> dict:
    starts = [datetime.fromisoformat(l["t"].replace("Z", "+00:00")) for l in lines if l["event"] == "tick_started"]
    gaps = [mins(a, b) for a, b in zip(starts[-11:], starts[-10:])]
    every = round(statistics.median(gaps)) if gaps else None
    since = round(mins(starts[-1], w.t)) if starts and w.t else None
    back = [l for l in lines if l["event"] == "returned" and l["actor"] == "foreman"]
    return dict(ticks=len(starts), every=every, since=since,
                next=max(0, every - since) if every is not None and since is not None else None,
                took_back=sum(l.get("reason") in ("quota", "stale") for l in back),
                preempted=sum(l.get("reason") == "preempted" for l in back),
                planned=len({l.get("parent") for l in lines if l["event"] == "filed" and l["actor"] == "foreman" and l.get("parent")}))


def release(lines: list[dict]) -> dict:
    pub = [l for l in lines if l["event"] == "published"]
    frozen = [l for l in lines if l["event"] == "frozen"]
    return dict(latest=pub[-1]["version"] if pub else None, latest_t=pub[-1]["t"] if pub else None,
                candidate=frozen[-1]["version"] if frozen and (not pub or frozen[-1]["version"] != pub[-1]["version"]) else None,
                pm_running=any(l["actor"] == "pm" for l in lines),
                pu_running=any(l["actor"] == "power_user" for l in lines))


QUOTA_RETRY = 30  # minutes: the watchdog's quota_retry
WATCHDOG_EVERY = 0.5  # minutes between watchdog steps


def wake(w: World, a: str, ready: bool) -> dict | None:
    """When an agent that is not working will next start: what the watchdog
    will do, and when. at: a time; None when it waits on an event."""
    st = w.state(M.AGENT, a) or "off"
    holds = a in w.holding
    if w.is_(M.QUOTA, a, "out"):
        at = w.since(M.QUOTA, a) + timedelta(minutes=QUOTA_RETRY)
        return dict(why="out of quota: the watchdog tries a session again", at=iso(at))
    if st == "working":
        return None
    if st == "idle":
        return dict(why="session running; it takes the queue's head itself", at=None)
    if holds or ready:
        return dict(why="the watchdog starts a session within 30 s", at=iso(w.t + timedelta(minutes=WATCHDOG_EVERY)) if w.t else None)
    return dict(why="asleep until an issue is ready", at=None)
