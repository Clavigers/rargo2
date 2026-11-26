"""Work that comes back waits for the agent that held it last; an agent may
ask the owner; work its holder keeps not moving is taken from it."""
import copy

import pytest

from factory import machines as M
from factory.machines import IssueS
from factory.world import Illegal
from test_world import C1, C2, CONFIG, SHA, X1, Log

RULES = copy.deepcopy(CONFIG)
RULES["foreman"] |= {"affinity_minutes": 120, "no_progress_sessions": 2, "session_minutes": 90}


def log():
    g = Log()
    g("human", "config_applied", config=RULES)
    for a in (C1, C2, X1):
        g("watchdog", "session_started", agent=a)
    g("human", "filed", issue=1, kind="enhancement")
    return g


def pushed_and_red():
    g = log()
    g(C1, "taken", issue=1)(C1, "push", issue=1, pr=10, push=1, sha=SHA)
    g("ci", "ci_done", pr=10, push=1, result="fail")
    return g


def test_red_work_waits_for_the_agent_that_pushed_it():
    g = pushed_and_red()
    assert g.w.reserved_for(1) == C1
    assert g.w.ready_queue(C1) == [1] and g.w.ready_queue(C2) == []
    with pytest.raises(Illegal, match="waits for claude-code-1"):
        g(C2, "taken", issue=1)
    g(C1, "taken", issue=1)


def test_after_the_allowance_anyone_takes_it():
    g = pushed_and_red()
    g(C2, "taken", issue=1, after=121)


def test_an_agent_without_quota_does_not_keep_work_waiting():
    g = pushed_and_red()
    g("watchdog", "session_ended", agent=C1, reason="quota")("watchdog", "quota_out", agent=C1)
    assert g.w.reserved_for(1) is None
    g(C2, "taken", issue=1)


def test_old_configs_have_no_affinity():
    g = Log().start()
    g("human", "filed", issue=1, kind="enhancement")
    g(C1, "taken", issue=1)(C1, "push", issue=1, pr=10, push=1, sha=SHA)
    g("ci", "ci_done", pr=10, push=1, result="fail")
    g(C2, "taken", issue=1)


def test_asking_the_owner_holds_the_issue_until_they_answer():
    g = log()
    g(C1, "taken", issue=1)
    with pytest.raises(Illegal, match="says what"):
        g(C1, "returned", issue=1, reason="asked")
    g(C1, "returned", issue=1, reason="asked", note="tabs or spaces?")
    assert g.w.is_(M.ISSUE, 1, IssueS.parked) and g.w.issues[1].asked
    g("human", "unparked", issue=1)
    assert not g.w.issues[1].asked and g.w.reserved_for(1) == C1  # it wakes the asker
    with pytest.raises(Illegal, match="waits for"):
        g(X1, "taken", issue=1)
    g(C1, "taken", issue=1)


def test_sessions_that_end_without_moving_the_issue_lose_it():
    g = log()
    g(C1, "taken", issue=1)
    g("watchdog", "session_ended", agent=C1, reason="finished")
    assert g.w.issues[1].stuck == {C1: 1}
    g("foreman", "tick_started", tick=1)
    with pytest.raises(Illegal, match="ended 1 sessions"):
        g("foreman", "returned", issue=1, reason="no_progress")
    g("foreman", "tick_ended", tick=1)
    g("watchdog", "session_started", agent=C1)("watchdog", "session_ended", agent=C1, reason="finished")
    g("foreman", "tick_started", tick=2)("foreman", "returned", issue=1, reason="no_progress")
    assert C1 in g.w.issues[1].given_up and g.w.reserved_for(1) is None
    g(C2, "taken", issue=1)  # another agent starts fresh


def test_a_session_that_pushed_is_not_stuck():
    g = log()
    g(C1, "taken", issue=1)
    g("watchdog", "session_ended", agent=C1, reason="finished")
    g("watchdog", "session_started", agent=C1)
    g(C1, "push", issue=1, pr=10, push=1, sha=SHA)
    assert g.w.issues[1].stuck == {}


def test_the_foreman_answers_a_question_and_tunes_the_factory():
    g = log()
    g(C1, "taken", issue=1)(C1, "returned", issue=1, reason="asked", note="tabs or spaces?")
    g("human", "filed", issue=2, kind="enhancement")
    with pytest.raises(Illegal, match="held by owner"):
        g("human", "parked", issue=2)("foreman", "unparked", issue=2)
    g("foreman", "unparked", issue=1)  # its answer is on the issue
    assert g.w.reserved_for(1) == C1
    g("human", "filed", issue=3, kind="enhancement")
    g("foreman", "parked", issue=3, reason="foreman")("foreman", "unparked", issue=3)
    more = copy.deepcopy(RULES) | {"version": 2}
    more["agents"][1]["count"] = 3
    more["foreman"]["orchestrate_minutes"] = 60
    g("foreman", "tick_started", tick=1)("foreman", "config_applied", config=more)
    assert "codex-3" in g.w.config.names()
    for change in ({"reviewers": ["other"]}, {"agents": [dict(more["agents"][0], count=5), more["agents"][1]]}):
        with pytest.raises(Illegal, match="the foreman"):
            g("foreman", "config_applied", config=more | {"version": 3} | change)


def test_quota_readings_are_in_the_log(tmp_path):
    import json
    from factory import usage
    from factory.ledger import Ledger
    path = str(tmp_path / "log.jsonl")
    Ledger(path).submit("human", json.dumps({"event": "config_applied", "config": RULES}))
    reading = {"plans": {"codex": {"windows": [{"name": "Week", "used": 19.0, "resets": "2026-12-03T03:45:23Z"}]},
                         "grok": {"windows": [], "error": "nothing read"}}}
    usage.record(reading, path)
    usage.record(reading, path)  # unchanged: not written again
    reading["plans"]["codex"]["windows"][0]["used"] = 21.0
    usage.record(reading, path)
    rows = [json.loads(x) for x in open(path).read().splitlines()][1:]
    assert [(r["event"], r["harness"], r["windows"][0]["used"]) for r in rows] == \
        [("quota_seen", "codex", 19.0), ("quota_seen", "codex", 21.0)]
