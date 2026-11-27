import json
import pathlib
from datetime import datetime, timedelta, timezone

import pytest
from pydantic import ValidationError

from factory import doc, machines as M
from factory.machines import IssueS
from factory.model import parse
from factory.world import Illegal, World

T0 = datetime(2026, 11, 25, tzinfo=timezone.utc)
SHA = "a" * 40
CONFIG = {
    "version": 1,
    "plans": [{"name": "max", "hours_per_week": 168.0}],
    "agents": [
        {"type": "claude-code", "count": 2, "model": "claude-opus-5-5", "plan": "max"},
        {"type": "codex", "count": 1, "model": "gpt-6.2-sol", "plan": "max"},
    ],
    "reviewers": ["placeholder"],
    "dictator": {"tick_minutes": 20, "preempt_minutes": 60},
    "release": {"freeze_every_days": 7, "uses_required": 1, "soak_minutes": 120},
    "guesses": ["release.soak_minutes"],
}
C1, C2, X1 = "claude-code-1", "claude-code-2", "codex-1"


class Log:
    """A log being written, checked line by line."""

    def __init__(self):
        self.w = World()
        self.minute = 0
        self.lines: list[str] = []

    def __call__(self, actor, event, after=1, **fields):
        self.minute += after
        t = (T0 + timedelta(minutes=self.minute)).strftime("%Y-%m-%dT%H:%M:%SZ")
        text = json.dumps({"schema": 1, "seq": self.w.seq + 1, "t": t,
                           "actor": actor, "event": event, **fields})
        line = parse(text)
        self.w = self.w.attempt(line)
        self.lines.append(line.dump())
        return self

    def start(self):
        self("human", "config_applied", config=CONFIG)
        for a in (C1, C2, X1):
            self("watchdog", "session_started", agent=a)
        return self

    def green(self, pr, push):
        self("ci", "ci_done", pr=pr, push=push, result="pass")
        self("reviewer", "review_done", by="placeholder", pr=pr, push=push, result="approved")
        return self

    def ship(self, issue, pr, by=C1):
        self(by, "taken", issue=issue)
        self(by, "push", issue=issue, pr=pr, push=1, sha=SHA)
        self.green(pr, 1)
        return self("merger", "merged", pr=pr, push=1, sha=SHA)

    def tick(self, *lines):
        n = self.w.tick + 1
        self("dictator", "tick_started", tick=n)
        for line in lines:
            self("dictator", *line[:1], **line[1])
        return self("dictator", "tick_ended", tick=n)


# ---------------------------------------------------------------- scenarios


def initiative_to_release():
    g = Log().start()
    g("human", "filed", issue=1, kind="initiative")
    g.tick(("filed", dict(issue=2, kind="enhancement", parent=1)),
           ("filed", dict(issue=3, kind="enhancement", parent=1, deps=[2])))
    assert g.w.state(M.ISSUE, 3) == IssueS.blocked
    g(C1, "taken", issue=2)
    g(C1, "push", issue=2, pr=10, push=1, sha=SHA)
    g("ci", "ci_done", pr=10, push=1, result="fail")
    assert g.w.needs_work(2)
    g(X1, "taken", issue=2)
    g(X1, "push", issue=2, pr=10, push=2, sha=SHA)
    g.green(10, 2)
    g("merger", "merged", pr=10, push=2, sha=SHA)
    assert g.w.state(M.ISSUE, 3) == IssueS.ready
    g.ship(3, 11)
    g("human", "closed", issue=1, reason="done")
    g("pm", "frozen", version="0.1.0", sha=SHA)
    g("pm", "built", version="0.1.0", build=1, sha=SHA)
    g("power_user", "use_started", use=1, version="0.1.0", build=1)
    g("power_user", "filed", issue=4, kind="bug")
    assert g.w.issues[4].found == ("use", 1)
    g("power_user", "use_ended", use=1)
    g("pm", "release_held", version="0.1.0", build=1, reasons=["known_bugs", "soak"])
    g.ship(4, 12)
    g("human", "attributed", bug=4, pr=11)
    g("pm", "backported", version="0.1.0", pr=12)
    g("pm", "release_held", version="0.1.0", build=1, reasons=["soak", "stale_build"])
    g("pm", "built", version="0.1.0", build=2, sha=SHA)
    g("power_user", "use_started", use=2, version="0.1.0", build=2)
    g("power_user", "use_ended", use=2, after=200)
    g("pm", "published", version="0.1.0", build=2)
    g("human", "use_started", use=3, version="0.1.0", build=2)
    g("human", "use_ended", use=3)
    return g


def agents_come_and_go():
    g = Log().start()
    g("human", "filed", issue=1, kind="enhancement")
    g("human", "filed", issue=2, kind="enhancement")
    g(C1, "taken", issue=1)
    g("watchdog", "quota_out", agent=C1)
    g("watchdog", "session_ended", agent=C1, reason="quota")
    g("watchdog", "quota_back", agent=C1)
    g("watchdog", "session_started", agent=C1)  # resumes #1
    g(C1, "returned", issue=1, reason="gave_up", note="needs a design")
    g("watchdog", "session_ended", agent=C2, reason="finished")  # idle -> off
    g(X1, "taken", issue=2)
    g("watchdog", "session_ended", agent=X1, reason="crashed")
    g.tick(("returned", dict(issue=2, reason="stale")))
    g("human", "closed", issue=1, reason="wontfix")
    return g


def checks_race():
    g = Log().start()
    g("human", "filed", issue=1, kind="enhancement")
    g("human", "filed", issue=2, kind="enhancement")
    g("human", "filed", issue=3, kind="enhancement")
    g(C1, "taken", issue=1)
    g(C1, "push", issue=1, pr=10, push=1, sha=SHA)
    g("reviewer", "review_done", by="placeholder", pr=10, push=1, result="changes", reasons=["tests"])
    g(C1, "taken", issue=1)
    g(C1, "push", issue=1, pr=10, push=2, sha=SHA)  # voids push 1's pending CI
    g.green(10, 2)
    g("merger", "merge_failed", pr=10, push=2, reason="conflict")
    g("human", "depends", issue=3, on=2)  # ready -> blocked
    g(C1, "taken", issue=2)
    g("human", "depends", issue=2, on=1)
    g(C1, "returned", issue=2, reason="blocked")  # held -> blocked
    g("human", "undepends", issue=2, on=1, note="not needed after all")
    g(C1, "taken", issue=2)
    g(C1, "push", issue=2, pr=11, push=1, sha=SHA)
    g("human", "depends", issue=2, on=1)
    g("ci", "ci_done", pr=11, push=1, result="fail")  # checking -> blocked; review void
    g("human", "closed", issue=2, reason="wontfix")  # blocked -> closed; pr abandoned
    g("human", "closed", issue=3, reason="duplicate", duplicate_of=1)  # ready -> closed
    return g


def outsiders_act():
    """The human and others act on GitHub; the factory follows."""
    g = Log().start()
    g("human", "filed", issue=1, kind="initiative")
    g("human", "filed", issue=2, kind="enhancement", parent=1)
    g("human", "filed", issue=3, kind="enhancement")
    g(C1, "taken", issue=2)
    g("human", "closed", issue=2, reason="completed")  # held -> closed
    assert C1 not in g.w.holding and g.w.state(M.AGENT, C1) == "idle"
    g(C1, "taken", issue=3)
    g(C1, "push", issue=3, pr=10, push=1, sha=SHA)
    g("outside", "merged", pr=10, push=1, sha=SHA)  # unreviewed, but it happened
    assert g.w.state(M.ISSUE, 3) == IssueS.closed
    g("human", "filed", issue=4, kind="enhancement", parent=1)
    g("outside", "closed", issue=1, reason="wontfix")  # open child and all
    return g


def owner_holds():
    """The owner parks and unparks issues in every state."""
    g = Log().start()
    g("human", "filed", issue=1, kind="enhancement")
    g("human", "filed", issue=2, kind="enhancement", deps=[1])
    g("human", "filed", issue=3, kind="enhancement")
    g("human", "parked", issue=3, reason="owner")  # ready -> parked
    with pytest.raises(Illegal, match="parked, not ready"):
        g(C1, "taken", issue=3)
    g("human", "unparked", issue=3)  # parked -> ready
    g("human", "parked", issue=2, reason="owner")  # blocked -> parked
    g("human", "unparked", issue=2)  # parked -> blocked
    g(C1, "taken", issue=3)
    g("human", "parked", issue=3, reason="owner")  # held: the agent keeps working
    assert g.w.state(M.ISSUE, 3) == IssueS.held
    g(C1, "returned", issue=3, reason="gave_up")  # held -> parked
    g("human", "unparked", issue=3)
    g(C1, "taken", issue=3)
    g(C1, "push", issue=3, pr=9, push=1, sha=SHA)
    g("human", "parked", issue=3, reason="owner")
    g("ci", "ci_done", pr=9, push=1, result="fail")  # checking -> parked
    assert g.w.state(M.ISSUE, 3) == IssueS.parked
    g("human", "closed", issue=3, reason="wontfix")  # parked -> closed
    return g


SCENARIOS = [initiative_to_release, agents_come_and_go, checks_race, outsiders_act,
             owner_holds]


@pytest.mark.parametrize("scenario", SCENARIOS)
def test_scenario(scenario):
    scenario()


def test_every_edge_is_reachable():
    """The scenarios walk every declared edge, so no machine has a dead one."""
    seen = set().union(*(s().w.moves() for s in SCENARIOS))
    declared = {(mc.name, None, str(s)) for mc in M.MACHINES for s in mc.initial} | \
               {(mc.name, str(a), str(b)) for mc in M.MACHINES for a, b in mc.edges}
    assert declared - seen == set()
    assert seen - declared == set()


def test_intervals_tile_each_life():
    """Each instance's intervals are back to back from creation, with no gap."""
    w = initiative_to_release().w
    for key in {(iv.machine, iv.key) for iv in w.intervals}:
        ivs = sorted((iv for iv in w.intervals + w.open_intervals()
                      if (iv.machine, iv.key) == key), key=lambda iv: iv.start)
        for a, b in zip(ivs, ivs[1:]):
            assert (a.end, a.end_t) == (b.start, b.start_t)
            assert a.end_t >= a.start_t


# ---------------------------------------------------------------- refusals


def one_held():
    g = Log().start()
    g("human", "filed", issue=1, kind="enhancement")
    return g(C1, "taken", issue=1)


@pytest.mark.parametrize("actor,event,fields,match", [
    (C2, "push", dict(issue=1, pr=5, push=1, sha=SHA), "not working on #1"),
    (C1, "push", dict(issue=1, pr=1, push=1, sha=SHA), "#1 is taken"),
    (C1, "push", dict(issue=1, pr=5, push=2, sha=SHA), "push 2 after 0"),
    (C1, "taken", dict(issue=1), "not idle"),
    (C2, "taken", dict(issue=1), "held, not ready"),
    ("human", "taken", dict(issue=1), "may not write"),
    ("grok-1", "taken", dict(issue=1), "no role and no agent"),
    ("dictator", "returned", dict(issue=1, reason="stale"), "decides only inside a tick"),
    (C1, "returned", dict(issue=1, reason="blocked"), "no open dependency"),
    (C2, "returned", dict(issue=1, reason="gave_up"), "only claude-code-1"),
    ("human", "closed", dict(issue=1, reason="done"), "only by merging"),
    ("power_user", "filed", dict(issue=2, kind="bug"), "open use session"),
    ("dictator", "tick_ended", dict(tick=1), "decides only inside a tick"),
    ("pm", "built", dict(version="0.1.0", build=1, sha=SHA), "not frozen"),
    ("ci", "ci_done", dict(pr=5, push=1, result="pass"), "no pr 5"),
    ("human", "config_applied", dict(config=CONFIG), "version 1 after 1"),
])
def test_refused(actor, event, fields, match):
    g = one_held()
    with pytest.raises(Illegal, match=match):
        g(actor, event, **fields)


def test_merge_needs_green_and_approved_on_the_same_push():
    g = one_held()
    g(C1, "push", issue=1, pr=5, push=1, sha=SHA)
    g("reviewer", "review_done", by="placeholder", pr=5, push=1, result="approved")
    with pytest.raises(Illegal, match="not green and approved"):
        g("merger", "merged", pr=5, push=1, sha=SHA)
    g("ci", "ci_done", pr=5, push=1, result="fail")
    with pytest.raises(Illegal, match="no edge fail -> pass"):
        g("ci", "ci_done", pr=5, push=1, result="pass")


def test_a_conflict_while_ci_is_pending_needs_work():
    g = one_held()
    g(C1, "push", issue=1, pr=5, push=1, sha=SHA)
    with pytest.raises(Illegal, match="not green and approved"):
        g("merger", "merge_failed", pr=5, push=1, reason="stale_base")  # only a conflict
    g("merger", "merge_failed", pr=5, push=1, reason="conflict")  # GitHub runs no CI on it
    w = g.w
    assert w.is_(M.CI, "5/1", "void") and w.is_(M.REVIEW, "5/1/placeholder", "void")
    assert w.needs_work(1)
    with pytest.raises(Illegal, match="not green and approved"):
        g("merger", "merge_failed", pr=5, push=1, reason="conflict")  # once


def test_failed_ci_voids_the_review():
    g = one_held()
    g(C1, "push", issue=1, pr=5, push=1, sha=SHA)
    g("ci", "ci_done", pr=5, push=1, result="fail")
    with pytest.raises(Illegal, match="no edge void -> approved"):
        g("reviewer", "review_done", by="placeholder", pr=5, push=1, result="approved")


def test_preemption():
    g = Log().start()
    for n in (1, 2, 3):
        g("human", "filed", issue=n, kind="enhancement")
    g(C1, "taken", issue=1)
    g(C2, "taken", issue=2)
    g(C1, "filed", issue=4, kind="bug")
    assert g.w.issues[4].found == ("work", 1)
    g("dictator", "tick_started", tick=1)
    with pytest.raises(Illegal, match="codex-1 is free"):
        g("dictator", "returned", issue=1, reason="preempted")
    g("dictator", "tick_ended", tick=1)
    g(X1, "taken", issue=3)
    g("dictator", "tick_started", tick=2)
    with pytest.raises(Illegal, match="no bug has been ready 60"):
        g("dictator", "returned", issue=1, reason="preempted")
    g("dictator", "tick_ended", tick=2)
    g("dictator", "tick_started", tick=3, after=60)
    g("dictator", "returned", issue=1, reason="preempted")


def test_config_cannot_drop_a_live_agent():
    g = Log().start()
    with pytest.raises(Illegal, match="drops codex-1"):
        g("human", "config_applied", config={**CONFIG, "version": 2,
                                             "agents": CONFIG["agents"][:1]})


def test_config_can_grow_a_type():
    g = Log().start()
    more = [{**CONFIG["agents"][0], "count": 3}, CONFIG["agents"][1]]
    g("human", "config_applied", config={**CONFIG, "version": 2, "agents": more})
    g("watchdog", "session_started", agent="claude-code-3")


def test_no_cycles():
    g = Log().start()
    g("human", "filed", issue=1, kind="enhancement")
    g("human", "filed", issue=2, kind="enhancement", deps=[1])
    with pytest.raises(Illegal, match="cycle"):
        g("human", "depends", issue=1, on=2)


def test_follow_up_work_stays_in_its_initiative():
    g = Log().start()
    g("human", "filed", issue=1, kind="initiative")
    g("human", "filed", issue=2, kind="initiative")
    g("human", "filed", issue=3, kind="enhancement", parent=1)
    g(C1, "taken", issue=3)
    with pytest.raises(Illegal, match="initiative"):
        g(C1, "filed", issue=4, kind="enhancement", parent=2)
    g(C1, "filed", issue=4, kind="enhancement", parent=1)


def test_attribution_is_restated_not_edited():
    g = Log().start()
    g("human", "filed", issue=1, kind="enhancement")
    g.ship(1, 5)
    g("human", "filed", issue=2, kind="enhancement")
    g.ship(2, 6)
    g("human", "filed", issue=4, kind="enhancement")
    g(C2, "taken", issue=4)
    g(C2, "filed", issue=3, kind="bug")
    g("human", "attributed", bug=3, pr=5)
    first = g.w.seq
    with pytest.raises(Illegal, match="replaces must be"):
        g("human", "attributed", bug=3, pr=6)
    g("human", "attributed", bug=3, pr=6, replaces=first)


def test_scope_freeze():
    g = Log().start()
    g("human", "scope_frozen")
    with pytest.raises(Illegal, match="scope is frozen"):
        g("human", "filed", issue=1, kind="enhancement")


# ---------------------------------------------------------------- shape


GOOD = {"schema": 1, "seq": 1, "t": "2026-11-25T00:00:00Z", "actor": "human",
        "event": "filed", "issue": 1, "kind": "enhancement"}


@pytest.mark.parametrize("patch,match", [
    ({"extra": 1}, "Extra inputs"),
    ({"t": "2026-11-25T00:00:00+00:00"}, "UTC to the second"),
    ({"t": "2026-11-25T00:00:00.5Z"}, "UTC to the second"),
    ({"issue": "1"}, "valid integer"),
    ({"issue": 1.0}, "valid integer"),
    ({"kind": "feature"}, "kind"),
    ({"schema": 2}, "schema"),
    ({"deps": [2, 2]}, "repeats"),
    ({"kind": "initiative", "parent": 3}, "no deps or parent"),
])
def test_malformed(patch, match):
    with pytest.raises((ValidationError, ValueError), match=match):
        parse(json.dumps(GOOD | patch))


def test_repeated_key():
    with pytest.raises(ValueError, match="a key repeats"):
        parse('{"schema":1,"schema":1}')


def test_config_shape():
    def config_line(config):
        return json.dumps({"schema": 1, "seq": 1, "t": "2026-11-25T00:00:00Z",
                           "actor": "human", "event": "config_applied", "config": config})
    twice = {**CONFIG, "agents": [CONFIG["agents"][0]] * 2}
    with pytest.raises(ValidationError, match="listed twice"):
        parse(config_line(twice))
    with pytest.raises(ValidationError, match="no single value"):
        parse(config_line({**CONFIG, "guesses": ["release"]}))
    with pytest.raises(ValidationError, match="unknown plan"):
        parse(config_line({**CONFIG, "plans": []}))


def test_lines_round_trip():
    text = json.dumps(GOOD | {"deps": [3, 2]}, separators=(",", ":"))
    line = parse(text)
    assert parse(line.dump()) == line
    assert json.loads(line.dump())["deps"] == [2, 3]


def test_an_off_agent_writes_nothing():
    g = one_held()
    g("watchdog", "session_ended", agent=C1, reason="crashed")
    with pytest.raises(Illegal, match="is off"):
        g(C1, "returned", issue=1, reason="gave_up")


def test_a_refused_line_changes_nothing():
    g = one_held()
    before = g.w
    with pytest.raises(Illegal):
        g(C1, "push", issue=1, pr=1, push=1, sha=SHA)
    assert g.w is before and before.seq == 6 and before.state(M.ISSUE, 1) == IssueS.held


def test_seq_and_time():
    g = Log().start()
    with pytest.raises(Illegal, match="before the line above"):
        g("human", "filed", after=-5, issue=1, kind="enhancement")


def test_doc_is_current():
    path = pathlib.Path(__file__).parent.parent / "DATA-MODEL.md"
    assert doc.splice(path.read_text()) == path.read_text(), "run python -m factory doc"


def test_check_reads_a_log_file(tmp_path):
    import subprocess
    import sys
    log = tmp_path / "log.jsonl"
    log.write_text("\n".join(initiative_to_release().lines) + "\n")
    run = lambda: subprocess.run([sys.executable, "-m", "factory", "check", str(log)],
                                 capture_output=True, text=True,
                                 cwd=pathlib.Path(__file__).parent.parent)
    ok = run()
    assert ok.returncode == 0 and "legal" in ok.stdout
    lines = log.read_text().splitlines()
    log.write_text("\n".join(lines[:5] + lines[6:]) + "\n")  # drop a line
    bad = run()
    assert bad.returncode == 1 and ":6: illegal: seq 7 after 5" in bad.stderr


def test_ready_queue_puts_needs_work_then_bugs_first():
    g = Log().start()
    g("human", "filed", issue=1, kind="enhancement")
    g("human", "filed", issue=2, kind="enhancement")
    g(C1, "taken", issue=2)
    g(C1, "push", issue=2, pr=9, push=1, sha=SHA)
    g("ci", "ci_done", pr=9, push=1, result="fail")
    g(C2, "taken", issue=1)
    g(C2, "filed", issue=3, kind="bug")
    g(C2, "returned", issue=1, reason="gave_up")
    assert g.w.ready_queue() == [2, 3, 1]
    assert g.w.agent_view(C1) == {"agent": C1, "state": "idle", "quota": "ok", "issue": None}
    g(C1, "taken", issue=2)
    assert g.w.agent_view(C1)["next_push"] == 2
    assert g.w.agent_view(C1)["last_push"] == {"sha": SHA, "ci": "fail",
                                               "reviews": {"placeholder": "void"}}


def test_the_dictator_closes_only_waiting_work():
    g = one_held()
    g("dictator", "tick_started", tick=1)
    with pytest.raises(Illegal, match="only waiting work"):
        g("dictator", "closed", issue=1, reason="wontfix")
    with pytest.raises(Illegal, match="completed"):
        g("dictator", "closed", issue=1, reason="completed")


def test_every_reviewer_must_approve():
    g = Log()
    g("human", "config_applied", config={**CONFIG, "reviewers": ["coderabbit", "greptile"]})
    g("watchdog", "session_started", agent=C1)
    g("human", "filed", issue=1, kind="enhancement")
    g(C1, "taken", issue=1)
    g(C1, "push", issue=1, pr=5, push=1, sha=SHA)
    g("ci", "ci_done", pr=5, push=1, result="pass")
    g("reviewer", "review_done", by="coderabbit", pr=5, push=1, result="approved")
    with pytest.raises(Illegal, match="not green and approved"):
        g("merger", "merged", pr=5, push=1, sha=SHA)
    with pytest.raises(Illegal, match="placeholder is not a reviewer"):
        g("reviewer", "review_done", by="placeholder", pr=5, push=1, result="approved")
    g("reviewer", "review_done", by="greptile", pr=5, push=1, result="approved")
    g("merger", "merged", pr=5, push=1, sha=SHA)


def test_the_human_reports_bugs_whenever():
    g = Log().start()
    g("human", "filed", issue=1, kind="bug")
    assert g.w.issues[1].found == ("report",)


def test_logs_already_written_still_check():
    """tests/golden.jsonl is the factory's real log as it was. A change to the
    model that makes it illegal breaks the logs out there; never edit the file
    to make this pass."""
    w = World()
    for text in (pathlib.Path(__file__).parent / "golden.jsonl").read_text().splitlines():
        w.apply(parse(text))
    assert w.seq >= 24


def test_status_reads_well():
    text = checks_race().w.status()
    assert "claude-code-1" in text and "closed" in text
    text = initiative_to_release().w.status()
    assert "issues" in text


def test_a_given_up_issue_waits_for_the_owner():
    g = one_held()
    g(C1, "returned", issue=1, reason="gave_up", note="needs a deploy")
    with pytest.raises(Illegal, match="gave up on #1"):
        g(C1, "taken", issue=1)
    assert g.w.ready_queue(C1) == [] and g.w.ready_queue(C2) == [1]
    g("human", "parked", issue=1)
    g("human", "unparked", issue=1)
    g(C1, "taken", issue=1)


def test_an_agent_closes_what_needs_no_change():
    g = one_held()
    with pytest.raises(Illegal, match="evidence"):
        g(C1, "closed", issue=1, reason="completed")
    with pytest.raises(Illegal, match="only as completed"):
        g(C1, "closed", issue=1, reason="wontfix", note="meh")
    with pytest.raises(Illegal, match="does not hold"):
        g(C2, "closed", issue=1, reason="completed", note="checked")
    g(C1, "closed", issue=1, reason="completed", note="every item holds on the site")
    assert g.w.state(M.ISSUE, 1) == IssueS.closed and C1 not in g.w.holding
    assert g.w.issues[1].closer == C1

