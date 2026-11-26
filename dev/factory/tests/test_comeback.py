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
