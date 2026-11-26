"""Discrete event simulation of the rargo software factory, on SimPy.

Every role is a SimPy process: a generator that reads top to bottom. Time is in minutes. The model is a set of pools and stations joined by edges;
work items (initiatives, issues, pull requests, reports, releases) move along
the edges. Everything the GUI shows comes from graph() and Sim.snapshot().
"""
import collections
import heapq
import itertools
import json
import math
import pathlib
import random

import simpy

DAY = 1440
WEEK = 7 * DAY
KINDS = ("claude", "codex", "grok")

# key, group, label, min, max, step, default, choices
PARAMS = [
    ("owner_sessions", "Human", "Sessions using rargo per day", 0, 24, 1, 4, None),
    ("owner_session_time", "Human", "Session length (min)", 10, 480, 10, 60, None),
    ("owner_awake", "Human", "Hours awake per day", 1, 24, 1, 16, None),
    ("owner_find", "Human", "Chance a session finds a given bug", 0, 1, 0.05, 0.15, None),
    ("requests_per_day", "Human", "Issues and initiatives filed per day", 0, 100, 1, 20, None),
    ("init_share", "Human", "Filed issues that are initiatives", 0, 1, 0.01, 0.3, None),
    ("init_median", "Human", "Median issues per initiative", 1, 30, 1, 4, None),
    ("init_sigma", "Human", "Spread of initiative size (log sd)", 0, 2, 0.01, 0.8, None),
    ("serial_mean", "Issue graph", "Average share of an initiative that must go in order", 0, 1, 0.05, 0.4, None),
    ("focus_areas", "Issue graph", "Code areas one initiative touches", 1, 6, 1, 2, None),
    ("foreman_tick", "Foreman", "Tick every (min)", 1, 480, 1, 20, None),
    ("backlog_target", "Foreman", "Unblocked feature issues kept ready", 1, 80, 1, 15, None),
    ("preempt_wait", "Foreman", "Pull an agent off lower-priority work for a bug waiting longer than (min, 0 = never)", 0, 600, 5, 60, None),
    ("claude_slots", "Workers", "Claude slots", 0, 8, 1, 1, None),
    ("claude_quota", "Workers", "Claude working hours per week", 0, 168, 0.5, 168, None),
    ("codex_slots", "Workers", "Codex slots", 0, 8, 1, 1, None),
    ("codex_quota", "Workers", "Codex working hours per week", 0, 168, 0.5, 30, None),
    ("grok_slots", "Workers", "Grok slots", 0, 8, 1, 1, None),
    ("grok_quota", "Workers", "Grok working hours per week", 0, 168, 0.5, 12, None),
    ("work_time", "Workers", "Mean work per PR (min)", 1, 240, 0.5, 9, None),
    ("restart_delay", "Workers", "Idle time before a free worker starts again (min)", 0, 30, 0.5, 0.5, None),
    ("rework_defect", "Workers", "Defect rate of rework, relative to first try", 0, 1, 0.01, 0.6, None),
    ("self_find", "Workers", "Chance a work session turns up a hidden bug", 0, 1, 0.01, 0.1, None),
    ("p_followup", "Issue graph", "Chance a work session files follow-up issues", 0, 0.5, 0.005, 0.08, None),
    ("p_blocker", "Issue graph", "Chance an issue turns out to need another one first", 0, 0.5, 0.01, 0.05, None),
    ("p_bug", "Workers", "PRs that carry a bug", 0, 1, 0.05, 0.2, None),
    ("p_incomplete", "Workers", "PRs that miss part of the issue", 0, 1, 0.01, 0.4, None),
    ("areas", "Workers", "Code areas", 1, 12, 1, 6, None),
    ("area_lock", "Workers", "One issue per area at a time", 0, 1, 1, 1, ["no", "yes"]),
    ("p_conflict", "Workers", "Conflict chance when areas overlap", 0, 1, 0.05, 0.4, None),
    ("ci_time", "CI", "CI run (min)", 1, 60, 0.5, 3.5, None),
    ("ci_slots", "CI", "Parallel CI runs", 1, 16, 1, 8, None),
    ("ci_flake", "CI", "CI runs that fail", 0, 0.5, 0.01, 0.05, None),
    ("gate_mode", "Review", "Review runs", 0, 2, 1, 1, ["never", "on every push, with CI", "after merge (reopens the issue)"]),
    ("gate_time", "Review", "Review time (min)", 0.5, 60, 0.5, 2, None),
    ("gate_slots", "Review", "Parallel reviews", 1, 8, 1, 1, None),
    ("catch_bug", "Review", "Bugs the review catches", 0, 1, 0.05, 0.4, None),
    ("catch_incomplete", "Review", "Incomplete work it catches", 0, 1, 0.05, 0.85, None),
    ("freeze_days", "Product manager", "Feature freeze every (days)", 0, 14, 0.05, 3, None),
    ("soak_hours", "Product manager", "Quiet hours before release", 0, 72, 1, 12, None),
    ("min_sessions", "Product manager", "Power-user sessions required", 0, 20, 1, 3, None),
    ("pu_slots", "Power user", "Power users", 0, 4, 1, 1, None),
    ("session_time", "Power user", "Session length (min)", 10, 480, 10, 90, None),
    ("find_rate", "Power user", "Chance a session finds a given bug", 0, 1, 0.05, 0.15, None),
    ("friction", "Power user", "Friction reports per session", 0, 5, 0.1, 0.5, None),
    ("claude_cost", "Cost and quality by agent", "Claude plan ($ per month)", 0, 500, 1, 20, None),
    ("claude_extra", "Cost and quality by agent", "Claude extra usage ($ per working hour)", 0, 50, 0.5, 0, None),
    ("codex_cost", "Cost and quality by agent", "Codex plan ($ per month)", 0, 500, 1, 20, None),
    ("grok_cost", "Cost and quality by agent", "Grok plan ($ per month)", 0, 500, 1, 0, None),
    ("claude_defect", "Cost and quality by agent", "Claude defect rate, relative", 0, 3, 0.05, 0.85, None),
    ("codex_defect", "Cost and quality by agent", "Codex defect rate, relative", 0, 3, 0.05, 1, None),
    ("grok_defect", "Cost and quality by agent", "Grok defect rate, relative", 0, 3, 0.05, 1.3, None),
]

PRESETS = {
    "Proposed": {},
    "Today": {
        "gate_mode": 2, "area_lock": 0,
        "freeze_days": 0.07, "soak_hours": 0, "min_sessions": 0, "pu_slots": 0, "restart_delay": 7.5,
    },
}

MODELS = {"claude": "Claude Opus 5.5", "codex": "GPT-6.2 Sol", "grok": "Grok 4.7"}

# Values measured from the real factory by calibrate.py replace the guesses above.
SOURCES = {
    "claude_cost": "the server's Claude login is a Pro plan, billed through Apple; $20 is the web price, "
                   "the App Store price may be higher. Extra usage is switched on for it and is not in this figure",
    "codex_cost": "the server's Codex login is ChatGPT Plus, $20 a month",
    "grok_cost": "free account, as the owner says",
}
try:
    _cal = json.loads(pathlib.Path(__file__).with_name("calibration.json").read_text())
except FileNotFoundError:
    _cal = {}
for _k, _v in _cal.get("params", {}).items():
    for _i, _p in enumerate(PARAMS):
        if _p[0] == _k:
            _val = min(_p[4], max(_p[3], round(_v["value"] / _p[5]) * _p[5]))
            PARAMS[_i] = _p[:6] + (round(_val, 4),) + _p[7:]
            SOURCES[_k] = _v["source"]
for _name, _over in _cal.get("presets", {}).items():
    for _k, _v in _over.items():
        PRESETS[_name][_k] = _v["value"]
        SOURCES[_name + ":" + _k] = _v["source"]

NODES = [
    # id, label, what it is
    ("human", "Human", "You, the only user. You file issues and initiatives straight into the tracker, and use the stable release in sessions while awake. A session can hit hidden bugs; you file those too."),
    ("tracker", "Issues", "The GitHub issue tracker. Everything is an issue: initiatives, enhancements, bugs and friction reports."),
    ("foreman", "Foreman", "Acts only on its tick. Splits initiatives into chains of partly serial sub-issues, each labelled with a t-shirt size, takes locks back from agents that stopped, and pulls an agent off lower-priority work when a bug has waited too long."),
    ("workers", "Agents", "Coding agents. Each locks one issue, writes the change and opens a pull request."),
    ("checks", "Checks running", "A push starts CI and review together on that commit. Green and approved merges; anything red moves the pull request to Needs work."),
    ("needswork", "Needs work", "Open pull requests whose last push failed CI or got changes requested. Agents take these before new issues; the next push restarts both checks."),
    ("main", "main", "The main branch. Hidden bugs are ones nobody has found yet."),
    ("pm", "Product manager", "Cuts a release branch at each feature freeze and tags the release once it is tested and quiet."),
    ("rc", "Release candidate", "The frozen release branch. Only bug fixes are backported to it."),
    ("stable", "Latest release", "The newest tagged release. This is what you use."),
    ("poweruser", "Power user", "An agent that uses the release candidate with only the docs, and files bug and friction issues straight into the tracker."),
]

EDGES = [
    # from, to, kind
    ("human", "tracker", "flow"),
    ("tracker", "foreman", "plan"),
    ("foreman", "tracker", "plan"),
    ("foreman", "workers", "plan"),
    ("tracker", "workers", "flow"),
    ("workers", "tracker", "report"),
    ("workers", "checks", "flow"),
    ("checks", "needswork", "rework"),
    ("needswork", "workers", "rework"),
    ("checks", "main", "flow"),
    ("main", "tracker", "rework"),
    ("main", "pm", "flow"),
    ("pm", "rc", "flow"),
    ("rc", "stable", "flow"),
    ("stable", "human", "flow"),
    ("rc", "poweruser", "flow"),
    ("poweruser", "tracker", "report"),
]

LABEL = {"feature": "enhancement", "bug": "bug", "friction": "friction"}


def defaults():
    return {p[0]: p[6] for p in PARAMS}


def graph():
    return {
        "nodes": [dict(id=i, label=l, about=a) for i, l, a in NODES],
        "edges": [dict(src=a, dst=b, kind=k) for a, b, k in EDGES],
        "params": [dict(key=k, group=g, label=l, min=lo, max=hi, step=st, default=d, choices=c,
                        source=SOURCES.get(k, ""))
                   for k, g, l, lo, hi, st, d, c in PARAMS],
        "reference": _cal.get("reference", {}),
        "presets": {name: {**defaults(), **over} for name, over in PRESETS.items()},
    }


class Slots:
    """A counted resource like simpy.Resource, except that its capacity is read
    on every use, so the page can change it while the simulation runs."""

    def __init__(self, env, capacity):
        self.env, self.capacity, self.busy, self.waiting = env, capacity, 0, collections.deque()

    def acquire(self):
        event = self.env.event()
        self.waiting.append(event)
        self.pump()
        return event

    def release(self):
        self.busy -= 1
        self.pump()

    def pump(self):
        while self.waiting and self.busy < self.capacity():
            self.busy += 1
            self.waiting.popleft().succeed()


class Sim:
    def __init__(self, params=None, seed=1, move_n=0):
        self.p = {**defaults(), **(params or {})}
        self.rng = random.Random(seed)
        self.env = simpy.Environment()
        self.wake = self.env.event()  # fires when the queue, a lock, a quota or a setting changes
        self.ids = itertools.count(1)
        self.moves = collections.deque(maxlen=600)
        self.move_n = move_n
        self.log = collections.deque(maxlen=60)
        self.requests, self.backlog, self.inbox = [], [], []
        self.inflight, self.inits, self.bugs, self.area_merge = {}, {}, {}, {}
        self.slots, self.pu = [], []
        self.owner_slot = dict(state="write", why="", t0=0, t1=0)
        self.last_planned = ""
        self.used = {k: 0.0 for k in KINDS}
        self.ci = Slots(self.env, lambda: self.p["ci_slots"])
        self.gate = Slots(self.env, lambda: self.p["gate_slots"])
        self.prs = {}  # open pull requests, by issue number
        self.rc = self.stable = None
        self.version = self.changes = self.issues_done = 0
        self.last_freeze = self.last_foreman = 0.0
        self.merge_times = collections.deque()
        self.cycles = collections.deque(maxlen=50)
        self.m = collections.Counter()
        self.by = {k: collections.Counter() for k in KINDS}  # per agent: work minutes, PRs, defects, issues
        self.series = {k: [] for k in ("ready", "blocked", "idle", "hidden", "shipped", "rate")}
        for role in (self.owner, self.foreman, self.product_manager, self.quota, self.sampler):
            self.env.process(role())
        self.poke()

    # --- engine ---
    @property
    def t(self):
        return self.env.now

    def run_until(self, t_end):
        if t_end > self.env.now:
            self.env.run(until=t_end)

    def poke(self):
        """Wakes every process that is waiting for something to change."""
        self.sync()
        old, self.wake = self.wake, self.env.event()
        old.succeed()

    def move(self, src, dst, kind):
        self.move_n += 1
        self.moves.append((self.move_n, src, dst, kind))

    def say(self, text):
        self.log.append((self.t, text))

    def logn(self, mean, cv=0.6):
        s2 = math.log(1 + cv * cv)
        return self.rng.lognormvariate(math.log(mean) - s2 / 2, math.sqrt(s2))

    # --- owner and foreman ---
    def poisson(self, mean):
        limit, n, x = math.exp(-mean), 0, self.rng.random()
        while x > limit:
            n, x = n + 1, x * self.rng.random()
        return n

    def file_request(self):
        p = self.p
        size, kind = 1, "feature"
        if self.rng.random() < p["init_share"]:
            size = self.rng.lognormvariate(math.log(max(1, p["init_median"])), p["init_sigma"])
            size, kind = max(1, min(50, round(size))), "initiative"
        self.m["filed"] += 1
        if size > 1:
            self.requests.append(dict(id=next(self.ids), size=size, born=self.t))
            self.move("human", "tracker", "initiative")
        else:
            self.add_issue("feature", 2, src="human")

    def owner(self):
        """The only user. While awake, always doing one of two things: using the
        latest release in a session (which can turn up hidden bugs, filed as
        issues), or writing issues and initiatives, filed one at a time."""
        env, me = self.env, self.owner_slot
        while True:
            p = self.p
            n, awake = max(1, p["owner_sessions"]), p["owner_awake"] * 60
            if env.now % DAY >= awake:
                me.update(state="sleep", t0=env.now, t1=env.now + DAY - env.now % DAY)
                yield env.timeout(me["t1"] - env.now)
                continue
            # A session using the latest release.
            if p["owner_sessions"] > 0:
                me.update(state="use", t0=env.now, t1=env.now + self.logn(p["owner_session_time"], 0.4))
                if self.stable:
                    self.move("stable", "human", "release")
                yield env.timeout(me["t1"] - env.now)
                self.m["owner_sessions"] += 1
                for b in self.bugs.values():
                    if b["stable"] and not b["found"] and self.rng.random() < p["owner_find"]:
                        b["found"] = True
                        self.m["wild"] += 1
                        self.add_issue("bug", 0, fixes=b["id"], src="human")
            # Then writing issues until the next session, or bedtime.
            write = max(10.0, awake / n - p["owner_session_time"])
            write = min(write, max(1.0, awake - env.now % DAY))
            me.update(state="write", t0=env.now, t1=env.now + write)
            k = self.poisson(p["requests_per_day"] / n)
            at = sorted(self.rng.uniform(0, write) for _ in range(k))
            last = 0.0
            for x in at:
                yield env.timeout(x - last)
                last = x
                self.file_request()
            yield env.timeout(write - last)

    def add_issue(self, kind, pri, init=None, fixes=None, area=None, deps=(), born=None, src="foreman", late=False):
        if area is None:
            area = self.rng.randrange(max(1, int(self.p["areas"])))
        issue = dict(id=next(self.ids), kind=kind, pri=pri, fixes=fixes, init=init, area=area, deps=list(deps),
                     born=self.t if born is None else born, late=late, where="queue", merged=False,
                     rework=False, start=0.0, incomplete=False, newbug=False, lastbug=None,
                     holder=None, stalled=None)
        if init is not None:
            init["issues"].append(issue)
            init["done_at"] = None
            self.inits[init["id"]] = init
        self.enqueue(issue)
        self.move(src, "tracker", kind)
        return issue

    def enqueue(self, issue):
        issue["queued"] = self.t
        self.backlog.append(issue)

    def blocked(self, issue):
        return any(not d["merged"] for d in issue["deps"])

    def split(self, req):
        """One request becomes a graph of issues. How much of an initiative has
        to go in order is drawn per initiative from a U-shaped distribution, so
        some fan out wide and others are close to one long chain."""
        p, rng = self.p, self.rng
        n, mean = req["size"], min(0.98, max(0.02, p["serial_mean"]))
        serial = rng.betavariate(mean * 1.2, (1 - mean) * 1.2) if n > 1 else 0
        areas = max(1, int(p["areas"]))
        init = dict(id=req["id"], issues=[], done_at=None,
                    focus=rng.sample(range(areas), min(areas, int(p["focus_areas"]))))
        for _ in range(n):
            made, deps = init["issues"], []
            if made and rng.random() < serial:
                deps.append(made[-1])
                if len(made) > 1 and rng.random() < serial * 0.3:
                    deps.append(rng.choice(made[:-1]))
            self.add_issue("feature", 2, init, area=rng.choice(init["focus"]), deps=deps, born=req["born"])
        self.move("tracker", "foreman", "initiative")
        self.m["planned"] += 1
        if n > 1:
            waiting = sum(1 for i in init["issues"] if i["deps"])
            self.say(f"Foreman split initiative #{init['id']} into {n} issues, {waiting} of them waiting on another")

    def foreman(self):
        yield self.env.timeout(1)
        while True:
            p = self.p
            self.clear_stale_locks()
            self.preempt()
            for _ in range(8):  # what one tick can plan
                ready = sum(1 for i in self.backlog if i["kind"] == "feature" and not self.blocked(i))
                if not self.requests or ready >= p["backlog_target"]:
                    break
                self.split(self.requests.pop(0))
            self.last_foreman = self.t
            self.poke()
            yield self.env.timeout(max(1, p["foreman_tick"]))

    # --- runner and workers ---
    def set_state(self, s, state, why):
        s["acc"][s["state"]] += self.t - s["since"]
        s.update(state=state, why=why, since=self.t)

    def sync(self):
        """Makes the worker and power-user slots match the settings."""
        for kind in KINDS:
            want = int(self.p[kind + "_slots"])
            have = [s for s in self.slots if s["kind"] == kind]
            for n in range(len(have), want):
                s = dict(kind=kind, name=f"{kind} {n + 1}", state="idle", why="idle: just started",
                         issue=None, t0=0, t1=0, free_at=0, since=self.t,
                         acc=dict(work=0.0, wait=0.0, idle=0.0, out=0.0))
                self.slots.append(s)
                s["proc"] = self.env.process(self.worker(s))
            for s in reversed(have):
                if len(have) > want and s["state"] in ("idle", "out"):
                    self.slots.remove(s)
                    have.remove(s)
        self.slots.sort(key=lambda s: KINDS.index(s["kind"]))
        want = int(self.p["pu_slots"])
        while len(self.pu) < want:
            self.pu.append(dict(state="idle", t0=0, t1=0))
            self.env.process(self.power_user(self.pu[-1]))
        for s in reversed(self.pu):
            if len(self.pu) > want and s["state"] == "idle":
                self.pu.remove(s)

    def pick(self, s):
        p = self.p
        busy = {i["area"] for i in self.inflight.values()}
        ready = sorted((i for i in self.backlog if not self.blocked(i)), key=lambda i: (i["pri"], i["born"]))
        issue = next((i for i in ready if not (p["area_lock"] and i["area"] in busy)), None)
        if issue is None:
            s["why"] = ("idle: area locked" if ready else
                        "idle: all blocked" if self.backlog else "idle: queue empty")
            return None
        self.backlog.remove(issue)
        self.move("tracker", "agent:" + s["name"], issue["kind"])
        return issue

    def worker(self, s):
        """The runner loop for one slot: take the best issue that is free, do it, repeat."""
        env = self.env
        while s in self.slots:
            if self.used[s["kind"]] >= self.p[s["kind"] + "_quota"] * 60:
                if s["state"] != "out":
                    self.say(f"{s['name']} is out of quota until the weekly reset")
                    self.set_state(s, "out", "out of quota")
                yield self.wake
                continue
            if s["state"] == "out":
                self.set_state(s, "idle", "")
            if s["free_at"] > env.now:
                s["why"] = "idle: not woken yet"
                yield env.timeout(s["free_at"] - env.now)
                continue
            issue = self.pick(s)
            if issue is None:
                yield self.wake
                continue
            yield from self.attempt(s, issue)

    def attempt(self, s, issue):
        """One agent on one issue. It opens a pull request and pushes to it until
        CI is green and review approves the same push, then it merges."""
        env, p, rng = self.env, self.p, self.rng
        issue.update(where="flight", holder=s, byname=s["name"])
        self.inflight[issue["id"]] = issue
        pr = self.prs.get(issue["id"])
        if pr is None:
            pr = dict(n=issue["id"], issue=issue, agent=s["name"], pushes=0, phase="coding",
                      ci=None, review=None, ci_t=(0, 0), review_t=(0, 0))
            self.prs[issue["id"]] = pr
        else:
            self.move("needswork", "agent:" + s["name"], "pr")
        pr.update(agent=s["name"], phase="coding")
        while True:
            rework = pr["pushes"] > 0 or issue["rework"]
            dur = self.logn(p["work_time"], 1.0) * (0.4 if rework else 1)
            stuck = pr["pushes"] == 0 and not issue["rework"] and rng.random() < p["p_blocker"]
            if stuck:
                dur *= 0.3
            pr["phase"] = "coding"
            self.set_state(s, "work", "fixing" if rework else "working")
            s.update(issue=issue, t0=env.now, t1=env.now + dur)
            issue["start"] = env.now
            if not (yield from self.spend(s, issue, dur)):
                self.prs.pop(issue["id"], None)  # closed: the foreman gave the issue to someone else
                return
            if stuck:
                # The issue needs something that nobody planned: file it, close
                # this pull request, and put the issue back behind the new one.
                first = self.add_issue("feature", issue["pri"], issue["init"], area=issue["area"],
                                       src="agent:" + s["name"], late=True)
                issue["deps"].append(first)
                issue["where"] = "queue"
                self.inflight.pop(issue["id"], None)
                self.prs.pop(issue["id"], None)
                self.enqueue(issue)
                self.m["found_work"] += 1
                break
            # A push: what this commit gets wrong.
            skill = p[s["kind"] + "_defect"]
            again = p["rework_defect"] if rework else 1
            issue["incomplete"] = rng.random() < p["p_incomplete"] * again * skill
            issue["newbug"] = rng.random() < p["p_bug"] * (1 if issue["kind"] == "feature" else 0.5) * skill
            issue["by"] = s["kind"]
            pr["pushes"] += 1
            self.m["pushes"] += 1
            self.by[s["kind"]]["prs"] += 1
            self.by[s["kind"]]["defects"] += issue["incomplete"] + issue["newbug"]
            if rng.random() < p["self_find"]:
                hidden = [b for b in self.bugs.values() if b["main"] and not b["found"]]
                if hidden:
                    bug = rng.choice(hidden)
                    bug["found"] = True
                    self.m["self_found"] += 1
                    self.add_issue("bug", 0, fixes=bug["id"], src="agent:" + s["name"])
            if pr["pushes"] == 1 and rng.random() < p["p_followup"]:
                for _ in range(2 if rng.random() < 0.25 else 1):
                    self.add_issue("feature", 2, issue["init"], area=issue["area"], src="agent:" + s["name"], late=True,
                                   deps=[issue] if rng.random() < 0.5 else [])
                    self.m["found_work"] += 1
            # The push starts CI and review together, and the agent moves on.
            self.move("agent:" + s["name"], "checks", "pr")
            issue["holder"] = None
            self.env.process(self.checks(pr))
            break
        self.set_state(s, "idle", "")
        s["issue"] = None
        if p["restart_delay"] > 0:
            s["free_at"] = env.now + rng.uniform(0.5, 1.5) * p["restart_delay"]
        self.poke()

    def checks(self, pr):
        """CI and review on the latest push. Red sends the issue back to the
        queue at top priority with its pull request still open."""
        env, p, rng, issue = self.env, self.p, self.rng, pr["issue"]
        pr.update(phase="checks", ci=None, review=None)
        ci = env.process(self.check(pr, "ci", self.ci, p["ci_time"], 0.3))
        rv = env.process(self.check(pr, "review", self.gate, p["gate_time"], 0.8)) if p["gate_mode"] == 1 else None
        yield ci
        green = rng.random() >= p["ci_flake"]
        if green and not p["area_lock"] and self.area_merge.get(issue["area"], -1) > issue["start"] \
                and rng.random() < p["p_conflict"]:
            green = False
            self.m["conflicts"] += 1
        pr["ci"] = "pass" if green else "fail"
        bad = not green
        if bad:
            self.m["ci_fails"] += 1
            if rv is not None and rv.is_alive:
                rv.interrupt()
                pr["review"] = "cancelled"
        elif rv is not None:
            yield rv
            self.m["reviews"] += 1
            if (issue["incomplete"] and rng.random() < p["catch_incomplete"]) or \
                    (issue["newbug"] and rng.random() < p["catch_bug"]):
                pr["review"] = "changes"
                self.m["rejects"] += 1
                bad = True
            else:
                pr["review"] = "approved"
        if bad:
            pr["phase"] = "needs work"
            issue.update(rework=True, pri=0, where="queue")
            self.inflight.pop(issue["id"], None)
            self.enqueue(issue)
            self.move("checks", "needswork", "rework")
            self.poke()
            return
        self.prs.pop(issue["id"], None)
        self.merge(issue, "checks")
        self.poke()

    def check(self, pr, key, slots, mean, cv):
        """One status check on the latest push: waits for a runner, then runs.
        A newer push cancels it."""
        env = self.env
        pr[key] = "queued"
        ev = slots.acquire()
        try:
            yield ev
            d = self.logn(mean, cv)
            pr[key], pr[key + "_t"] = "running", (env.now, env.now + d)
            yield env.timeout(d)
        except simpy.Interrupt:
            if ev.triggered:
                slots.release()
            else:
                slots.waiting.remove(ev)
            return
        slots.release()

    def spend(self, s, issue, dur):
        """Works for dur minutes of quota. If the quota runs out first, the
        worker stops with the issue still locked. Nothing frees that lock until
        the quota comes back or the foreman, on a tick, takes the issue away.
        True when the work is done, False when the issue was taken away."""
        env, kind = self.env, s["kind"]
        out = lambda: self.p[kind + "_quota"] * 60 - self.used[kind] <= 1e-6
        while True:
            step = min(dur, max(0.0, self.p[kind + "_quota"] * 60 - self.used[kind]))
            self.used[kind] += step
            self.by[kind]["work"] += step
            dur -= step
            if step > 0:
                began = env.now
                try:
                    yield env.timeout(step)
                except simpy.Interrupt:  # the foreman pulled this agent off
                    self.used[kind] -= step - (env.now - began)
                    self.by[kind]["work"] -= step - (env.now - began)
                    return False
            if dur <= 1e-9:
                return True
            issue["stalled"] = env.now
            self.m["stalls"] += 1
            self.set_state(s, "out", "out, holds a lock")
            self.say(f"{s['name']} ran out of quota part-way through an issue and still holds its lock")
            while out():
                yield self.wake
                if issue["holder"] is not s:
                    return False
            self.m["stall_min"] += env.now - issue["stalled"]
            issue["stalled"] = None
            self.set_state(s, "work", "working")
            s.update(t0=env.now, t1=env.now + dur)

    def preempt(self):
        """The foreman's other check: a bug that has waited too long while every
        agent is busy takes the agent furthest from done on lower-priority work.
        That issue goes back to the queue and its work so far is lost."""
        wait = self.p["preempt_wait"]
        if wait <= 0 or any(s["state"] == "idle" for s in self.slots):
            return
        urgent = [i for i in self.backlog if i["pri"] == 0 and i["kind"] == "bug" and not self.blocked(i)
                  and self.t - i["queued"] >= wait]
        victims = sorted((s for s in self.slots if s["state"] == "work" and s["issue"] and s["issue"]["pri"] >= 1
                          and s["issue"]["stalled"] is None),
                         key=lambda s: (self.t - s["t0"]) / max(1e-9, s["t1"] - s["t0"]))
        for bug, s in zip(urgent, victims):
            issue = s["issue"]
            self.m["preempted"] += 1
            self.say(f"Foreman pulled {s['name']} off #{issue['id']} for bug #{bug['id']}, "
                     f"waiting {int(self.t - bug['queued'])} min")
            self.move("foreman", "agent:" + s["name"], "bug")
            self.move("agent:" + s["name"], "tracker", issue["kind"])
            issue.update(holder=None, where="queue")
            self.inflight.pop(issue["id"], None)
            self.enqueue(issue)
            s["issue"] = None
            self.set_state(s, "idle", "")
            s["proc"].interrupt()

    def clear_stale_locks(self):
        """The foreman's check: an issue locked by a worker that has stopped goes
        back to the queue. The work done so far is lost with the worker."""
        for issue in [i for i in self.inflight.values() if i["stalled"] is not None]:
            holder = issue["holder"]
            self.m["stall_min"] += self.t - issue["stalled"]
            self.m["unlocked"] += 1
            self.say(f"Foreman took an issue back from {holder['name']}, locked "
                     f"{int(self.t - issue['stalled'])} min by a stopped worker")
            holder.update(issue=None, why="out of quota")
            issue.update(stalled=None, holder=None, where="queue")
            self.inflight.pop(issue["id"], None)
            self.enqueue(issue)
            self.move("agent:" + holder["name"], "tracker", issue["kind"])

    def review(self, issue):
        """The after-merge review. True when it finds the work wanting."""
        env, p = self.env, self.p
        yield self.gate.acquire()
        yield env.timeout(self.logn(p["gate_time"], 0.8))
        self.gate.release()
        self.m["reviews"] += 1
        return ((issue["incomplete"] and self.rng.random() < p["catch_incomplete"])
                or (issue["newbug"] and self.rng.random() < p["catch_bug"]))

    def post_review(self, issue):
        if (yield from self.review(issue)) and issue["where"] == "done":
            self.m["reopens"] += 1
            issue.update(rework=True, pri=0, where="queue")
            if issue["lastbug"] is not None:
                issue["fixes"] = issue["lastbug"]
            if issue["init"] is not None:
                issue["init"]["done_at"] = None
                self.inits[issue["init"]["id"]] = issue["init"]
            self.enqueue(issue)
            self.move("main", "tracker", "rework")
            self.poke()

    def gc(self, bug):
        if not (bug["main"] or bug["rc"] or bug["stable"]):
            self.bugs.pop(bug["id"], None)

    def merge(self, issue, src):
        self.move(src, "main", issue["kind"])
        self.m["merged"] += 1
        self.changes += 1
        self.merge_times.append(self.t)
        self.cycles.append(self.t - issue["born"])
        self.issues_done += not issue["merged"]
        self.by[issue["by"]]["issues"] += not issue["merged"]
        issue.update(merged=True, where="done")
        self.area_merge[issue["area"]] = self.t
        self.inflight.pop(issue["id"], None)
        init = issue["init"]
        if init is not None and all(i["where"] == "done" for i in init["issues"]):
            init["done_at"] = self.t
        made = issue["newbug"]
        if issue["fixes"] is None:
            made = made or issue["incomplete"]
        else:
            bug = self.bugs.get(issue["fixes"])
            if bug and issue["incomplete"]:
                bug["found"] = False  # believed fixed, still there
                self.m["bad_fixes"] += 1
            elif bug:
                bug["main"] = False
                if bug["rc"] and self.rc:
                    self.env.process(self.backport(bug["id"]))
                self.gc(bug)
        issue["lastbug"] = None
        if made:
            bid = next(self.ids)
            self.bugs[bid] = dict(id=bid, main=True, rc=False, stable=False, found=False)
            issue["lastbug"] = bid
        if self.p["gate_mode"] == 2:
            self.env.process(self.post_review(issue))
        self.poke()

    # --- product manager, power user, customers ---
    def backport(self, bid):
        yield self.env.timeout(20)
        bug = self.bugs.get(bid)
        if bug and bug["rc"] and self.rc:
            bug["rc"] = False
            self.rc["changed"] = self.t
            self.rc["build"] += 1
            self.m["backports"] += 1
            self.move("main", "rc", "bug")
            self.gc(bug)

    def product_manager(self):
        while True:
            yield self.env.timeout(30)
            p = self.p
            if self.rc:
                known = any(b["rc"] and b["found"] for b in self.bugs.values())
                quiet = self.t - self.rc["changed"] >= p["soak_hours"] * 60
                tested = p["pu_slots"] == 0 or self.rc["sessions"] >= p["min_sessions"]
                if not known and quiet and tested:
                    shipped = 0
                    for b in list(self.bugs.values()):
                        shipped += b["rc"] and not b["stable"]  # count each bug once
                        b["stable"], b["rc"] = b["rc"], False
                        self.gc(b)
                    self.m["shipped"] += shipped
                    self.m["releases"] += 1
                    self.stable = dict(ver=self.rc["ver"], t=self.t, changes=self.rc["changes"])
                    self.say(f"Released {self.rc['ver']}: {self.rc['changes']} changes, "
                             f"{shipped} new unnoticed bugs shipped")
                    self.rc = None
                    self.move("rc", "stable", "release")
            if not self.rc and self.changes and self.t - self.last_freeze >= p["freeze_days"] * DAY:
                self.version += 1
                self.rc = dict(ver=f"0.8.{self.version}", changed=self.t, changes=self.changes,
                               sessions=0, build=1)
                for b in self.bugs.values():
                    b["rc"] = b["main"]
                self.say(f"Feature freeze: cut {self.rc['ver']} with {self.changes} changes")
                self.changes, self.last_freeze = 0, self.t
                self.move("main", "pm", "release")
                self.move("pm", "rc", "release")
                self.poke()

    def power_user(self, s):
        """Tests the release candidate the way a customer would, one session after another."""
        env = self.env
        while s in self.pu:
            if not self.rc:
                yield self.wake
                continue
            p, ver = self.p, self.rc["ver"]
            s.update(state="work", t0=env.now, t1=env.now + self.logn(p["session_time"], 0.3))
            self.move("rc", "poweruser", "release")
            yield env.timeout(s["t1"] - env.now)
            s["state"] = "idle"
            if not (self.rc and self.rc["ver"] == ver):
                continue
            self.rc["sessions"] += 1
            for b in self.bugs.values():
                if b["rc"] and not b["found"] and self.rng.random() < p["find_rate"]:
                    b["found"] = True
                    self.m["pu_found"] += 1
                    self.add_issue("bug", 0, fixes=b["id"], src="poweruser")
            for _ in range(self.poisson(p["friction"])):
                self.add_issue("friction", 1, src="poweruser")
                self.m["friction_filed"] += 1

    def quota(self):
        while True:
            yield self.env.timeout(WEEK)
            self.used = {k: 0.0 for k in KINDS}
            self.say("Weekly quota reset")
            self.poke()

    # --- reporting ---
    def rate(self):
        while self.merge_times and self.merge_times[0] < self.t - DAY:
            self.merge_times.popleft()
        return len(self.merge_times)

    def counts(self):
        bugs = self.bugs.values()
        return dict(
            hidden=sum(1 for b in bugs if b["main"] and not b["found"]),
            rc_known=sum(1 for b in bugs if b["rc"] and b["found"]),
            rc_unknown=sum(1 for b in bugs if b["rc"] and not b["found"]),
            live=sum(1 for b in bugs if b["stable"] and not b["found"]),
        )

    def sampler(self):
        while True:
            yield self.env.timeout(60)
            c = self.counts()
            blocked = sum(1 for i in self.backlog if self.blocked(i))
            idle = sum(1 for s in self.slots if s["state"] == "idle")
            for key, v in (("ready", len(self.backlog) - blocked), ("blocked", blocked), ("idle", idle),
                           ("hidden", c["hidden"]), ("shipped", c["live"]), ("rate", self.rate())):
                self.series[key].append(v)
                if len(self.series[key]) > 24 * 90:
                    del self.series[key][0]
            for k, init in list(self.inits.items()):  # forget finished initiatives after two hours
                if init["done_at"] is not None and self.t - init["done_at"] > 120:
                    del self.inits[k]

    def agents(self):
        """What each agent costs and what it produces."""
        p, months, out = self.p, self.t / (30 * DAY), []
        for k in KINDS:
            b = self.by[k]
            monthly = p[k + "_cost"] if p[k + "_slots"] else 0
            extra = p["claude_extra"] * b["work"] / 60 if k == "claude" else 0
            spent = monthly * months + extra
            out.append(dict(name=k, model=MODELS[k], monthly=monthly, spent=round(spent, 2),
                            hours=round(b["work"] / 60, 1), issues=b["issues"], prs=b["prs"],
                            defects=round(100 * b["defects"] / b["prs"], 1) if b["prs"] else None,
                            per_issue=round(spent / b["issues"], 2) if b["issues"] else None))
        return out

    def shares(self, s):
        acc = dict(s["acc"])
        acc[s["state"]] += self.t - s["since"]
        total = sum(acc.values()) or 1
        return {k: v / total for k, v in acc.items()}

    def metrics(self):
        m, c = self.m, self.counts()
        cycle = sum(self.cycles) / len(self.cycles) / 60 if self.cycles else 0
        days = max(self.t / DAY, 1e-9)
        sh = [self.shares(s) for s in self.slots]
        avg = lambda k: round(100 * sum(x[k] for x in sh) / len(sh)) if sh else 0
        spent = sum(a["spent"] for a in self.agents())
        return [
            ["Spent so far ($)", round(spent, 2), ""],
            ["Spend per 30 days ($)", round(spent / max(self.t / (30 * DAY), 1e-9), 2), ""],
            ["Cost per finished issue ($)", round(spent / max(1, self.issues_done), 3), ""],
            ["Merged PRs", m["merged"], ""],
            ["Pushes per merged PR", round(m["pushes"] / max(1, m["merged"]), 2), ""],
            ["CI failures", m["ci_fails"], ""],
            ["Review requested changes", m["rejects"], ""],
            ["Merged PRs per day, whole run", round(m["merged"] / days, 1), ""],
            ["Merged in the last 24 h", self.rate(), ""],
            ["Issues finished per day, whole run", round(self.issues_done / days, 1), "good"],
            ["PRs per finished issue", round(m["merged"] / max(1, self.issues_done), 2), ""],
            ["Issues found during work", m["found_work"], ""],
            ["Reviews that sent work back (%)", round(100 * (m["rejects"] + m["reopens"]) / max(1, m["reviews"])), ""],
            ["Request to merged (h, last 50)", round(cycle, 1), ""],
            ["Worker time working (%)", avg("work"), "good"],
            ["Worker time waiting for merge (%)", avg("wait"), ""],
            ["Worker time idle (%)", avg("idle"), "warn"],
            ["Worker time out of quota (%)", avg("out"), "bad"],
            ["Workers stopped mid-issue by quota", m["stalls"], ""],
            ["Locks the foreman took back", m["unlocked"], ""],
            ["Agents the foreman pulled off for a bug", m["preempted"], ""],
            ["Hours issues sat locked by a stopped worker", round(m["stall_min"] / 60, 1), "warn"],
            ["Merge conflicts", m["conflicts"], ""],
            ["Releases", m["releases"], ""],
            ["Bugs hiding in main", c["hidden"], "warn"],
            ["Bugs found by workers", m["self_found"], ""],
            ["Bugs found by the power user", m["pu_found"], "good"],
            ["Bugs shipped unnoticed (total)", m["shipped"], "warn"],
            ["Bugs you found in a release", m["wild"], "bad"],
        ]

    def graphs(self):
        """The live issue graph of each initiative that still has open work."""
        out = []
        for init in sorted(self.inits.values(), key=lambda i: (i["done_at"] is not None, i["id"])):
            issues = init["issues"]
            if len(issues) < 2:
                continue
            depth = {}
            for i in issues:  # issues are in creation order, except a late prerequisite
                depth[i["id"]] = 0
            for _ in range(3):
                for i in issues:
                    depth[i["id"]] = max([depth.get(d["id"], 0) + 1 for d in i["deps"]], default=0)
            out.append(dict(id=init["id"], done=init["done_at"] is not None,
                            late=sum(1 for i in issues if i["late"]),
                            issues=[dict(id=i["id"], deps=[d["id"] for d in i["deps"]], depth=depth[i["id"]],
                                         late=i["late"],
                                         state="stalled" if i["stalled"] is not None else "work" if i["where"] == "flight" else "done" if i["where"] == "done"
                                         else "blocked" if self.blocked(i) else "ready")
                                    for i in issues[:90]]))
            if len(out) == 10:
                break
        return out

    def chip(self, i, **extra):
        return dict(n=i["id"], t=LABEL[i["kind"]], rw=i["rework"], **extra)

    def snapshot(self):
        p, c, now = self.p, self.counts(), self.t

        def prog(x):
            return min(1, (now - x["t0"]) / max(1e-9, x["t1"] - x["t0"])) if x["state"] in ("work", "use", "write", "sleep") else 0

        def stage(i):
            if i["stalled"] is not None:
                return "stalled"
            pr = self.prs.get(i["id"])
            return pr["phase"] if pr else "work"

        queue = sorted((i for i in self.backlog if i["id"] not in self.prs), key=lambda i: (i["pri"], i["born"]))
        inits = []
        for init in self.inits.values():
            if init["done_at"] is None and len(init["issues"]) > 1:
                done = sum(1 for i in init["issues"] if i["where"] == "done")
                inits.append(dict(n=init["id"], done=done, total=len(init["issues"]),
                                  late=sum(1 for i in init["issues"] if i["late"])))
        cost = {a["name"]: a for a in self.agents()}
        agents = []
        for sl in self.slots:
            k = sl["kind"]
            agents.append(dict(name=sl["name"], kind=k, model=MODELS[k], state=sl["state"], why=sl["why"],
                               progress=prog(sl), share=self.shares(sl),
                               issue=self.chip(sl["issue"]) if sl["issue"] else None,
                               used=round(self.used[k] / 60, 1), quota=p[k + "_quota"],
                               monthly=cost[k]["monthly"], defects=cost[k]["defects"]))
        awake = now % DAY < p["owner_awake"] * 60
        me, rc, st = self.owner_slot, self.rc, self.stable
        mode = int(p["gate_mode"])
        board = dict(
            human=dict(state=me["state"], progress=prog(me), awake=awake, sessions=self.m["owner_sessions"],
                       found=self.m["wild"], filed=self.m["filed"], uses=st["ver"] if st else None, why=me["why"]),
            tracker=dict(
                planning=[dict(n=r["id"], size=r["size"]) for r in self.requests],
                initiatives=sorted(inits, key=lambda x: x["n"]),
                ready=[self.chip(i) for i in queue if not self.blocked(i)],
                blocked=[self.chip(i, on=[d["id"] for d in i["deps"] if not d["merged"]]) for i in queue if self.blocked(i)],
                progress=[self.chip(i, agent=i.get("byname", ""), stage=stage(i)) for i in self.inflight.values()],
                closed=self.issues_done),
            foreman=dict(next=max(0, self.last_foreman + p["foreman_tick"] - now), tick=p["foreman_tick"],
                         planned=self.m["planned"], unlocked=self.m["unlocked"], preempted=self.m["preempted"]),
            agents=agents,
            prs=dict(
                open=[dict(self.chip(pr["issue"]), agent=pr["agent"], pushes=pr["pushes"], phase=pr["phase"],
                           ci=pr["ci"], review=pr["review"] if mode == 1 else "off",
                           ci_p=min(1, (now - pr["ci_t"][0]) / max(1e-9, pr["ci_t"][1] - pr["ci_t"][0])),
                           review_p=min(1, (now - pr["review_t"][0]) / max(1e-9, pr["review_t"][1] - pr["review_t"][0])))
                      for pr in sorted(self.prs.values(), key=lambda x: x["agent"])],
                ci=dict(slots=int(p["ci_slots"]), busy=self.ci.busy, queued=len(self.ci.waiting), fails=self.m["ci_fails"]),
                review=dict(mode=["off", "on every push", "after merge"][mode], slots=int(p["gate_slots"]) if mode else 0,
                            busy=self.gate.busy, queued=len(self.gate.waiting), changes=self.m["rejects"], reopens=self.m["reopens"]),
                pushes=self.m["pushes"]),
            main=dict(merged=self.m["merged"], since=self.changes, hidden=c["hidden"],
                      known=sum(1 for b in self.bugs.values() if b["main"] and b["found"])),
            pm=dict(releases=self.m["releases"],
                    freeze=None if rc else max(0, self.last_freeze + p["freeze_days"] * DAY - now),
                    soak=max(0, rc["changed"] + p["soak_hours"] * 60 - now) if rc else None),
            rc=dict(ver=rc["ver"], known=c["rc_known"], hidden=c["rc_unknown"], sessions=rc["sessions"],
                    need=int(p["min_sessions"]), build=rc["build"], changes=rc["changes"]) if rc else None,
            stable=dict(ver=st["ver"], hidden=c["live"], age=now - st["t"], changes=st["changes"]) if st else None,
            poweruser=dict(slots=[dict(state=x["state"], progress=prog(x)) for x in self.pu],
                           found=self.m["pu_found"], friction=self.m["friction_filed"]),
        )
        return dict(t=now, board=board, metrics=self.metrics(), workers=dict(collections.Counter(x["state"] for x in self.slots)),
                    graphs=self.graphs(), agents=self.agents(), log=[[t, x] for t, x in list(self.log)[-14:]])
