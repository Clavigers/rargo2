"""The power user, against a real log."""
import copy
import json
from datetime import datetime, timedelta, timezone

from factory import machines as M
from factory.ledger import Ledger
from factory.poweruser import PowerUser
from test_world import CONFIG, SHA

T0 = datetime(2026, 1, 5, 12, 0, tzinfo=timezone.utc)


class Factory:
    def __init__(self, tmp, uses_required=1, during=None, use_gap_minutes=None):
        self.now = T0
        self.ledger = Ledger(tmp / "log.jsonl", clock=lambda: self.now)
        cfg = copy.deepcopy(CONFIG)
        cfg["release"] = {"freeze_every_days": 7, "uses_required": uses_required, "soak_minutes": 0}
        if use_gap_minutes is not None:
            cfg["release"]["use_gap_minutes"] = use_gap_minutes
        self("human", "config_applied", config=cfg)
        self.sessions, self.said, self.during, self.lost, self.exit = [], [], during, False, 0
        self.on_github = []
        self.pu = PowerUser(self.ledger, self, tmp / "use", "prompt", clock=self.clock,
                            poll=0, run=self.session, say=self.said.append)

    def clock(self):
        return self.now

    def issues(self, since):  # GitHub, and the observer catching up as the power user waits
        self.now += timedelta(minutes=1)
        for i in self.on_github:
            if i["number"] not in self.ledger.refresh().issues and not self.lost:
                self("power_user", "filed", issue=i["number"], kind="bug")
        return self.on_github

    def __call__(self, actor, event, **fields):
        self.now += timedelta(minutes=1)
        return self.ledger.submit(actor, json.dumps({"event": event, **fields}))

    def session(self, use, version, build, sha):
        self.sessions.append((use, version, build, sha))
        if self.during:
            self.during(self)
        return self.exit

    def candidate(self, version="0.7.11"):
        self("pm", "frozen", version=version, sha=SHA)
        self("pm", "built", version=version, build=1, sha=SHA)
        return self

    def events(self):
        return [json.loads(l)["event"] for l in open(self.ledger.path)][1:]


def test_nothing_to_use_without_a_build(tmp_path):
    f = Factory(tmp_path)
    f.pu.step()
    f("pm", "frozen", version="0.7.11", sha=SHA)
    f.pu.step()
    assert f.sessions == []


def test_it_uses_a_build_once_and_the_release_is_no_longer_held_for_uses(tmp_path):
    f = Factory(tmp_path).candidate()
    assert "uses" in f.ledger.world.hold_reasons("0.7.11")
    f.pu.step()
    assert f.sessions == [(1, "0.7.11", 1, SHA)]
    assert f.events()[-2:] == ["use_started", "use_ended"]
    assert "uses" not in f.ledger.refresh().hold_reasons("0.7.11")
    f.now += timedelta(hours=4)
    f.pu.step()  # that build has had its use
    assert len(f.sessions) == 1


def test_what_it_files_is_found_in_its_use(tmp_path):
    f = Factory(tmp_path, during=lambda f: f("power_user", "filed", issue=7, kind="bug")).candidate()
    f.pu.step()
    assert f.ledger.world.issues[7].found == ("use", 1)
    assert "known_bugs" in f.ledger.world.hold_reasons("0.7.11")


def test_the_use_stays_open_until_the_log_has_what_the_session_filed(tmp_path):
    def files(f):
        f.on_github.append({"number": 9, "body": "x\nFound in 0.7.11 build 1 (a), use session 1.\n"
                                                 "<!-- factory-actor: power_user -->"})
    f = Factory(tmp_path, during=files).candidate()
    f.pu.step()
    assert f.ledger.world.issues[9].found == ("use", 1)
    assert "power user: use 1 filed [9]" in f.said


def test_an_issue_the_log_never_gets_does_not_hold_the_use_open_for_ever(tmp_path):
    def files(f):
        f.lost = True
        f.on_github.append({"number": 9, "body": "use session 1.\n<!-- factory-actor: power_user -->"})
    f = Factory(tmp_path, during=files).candidate()
    f.pu.clock = lambda: (setattr(f, "now", f.now + timedelta(minutes=5)), f.now)[1]
    f.pu.step()
    assert f.ledger.world.is_(M.USE, 1, "ended") and any("never got [9]" in s for s in f.said)


def test_a_new_build_is_used_but_not_before_the_gap(tmp_path):
    f = Factory(tmp_path).candidate()
    f.pu.step()
    f("pm", "built", version="0.7.11", build=2, sha="b" * 40)
    f.pu.step()
    assert len(f.sessions) == 1
    f.now += timedelta(hours=3)
    f.pu.step()
    assert f.sessions[-1] == (2, "0.7.11", 2, "b" * 40)


def test_the_gap_is_the_configs_when_it_sets_one(tmp_path):
    f = Factory(tmp_path, use_gap_minutes=15).candidate()
    f.pu.step()
    f("pm", "built", version="0.7.11", build=2, sha="b" * 40)
    f.now += timedelta(minutes=5)
    f.pu.step()
    assert len(f.sessions) == 1
    f.now += timedelta(minutes=15)
    f.pu.step()
    assert f.sessions[-1] == (2, "0.7.11", 2, "b" * 40)


def test_a_failed_session_is_tried_again_after_the_gap_when_that_is_shorter(tmp_path):
    f = Factory(tmp_path, use_gap_minutes=10).candidate()
    f.exit = 1
    f.pu.step()
    assert f.ledger.world.uses[1].void
    f.exit = 0
    f.now += timedelta(minutes=5)
    f.pu.step()
    assert len(f.sessions) == 1
    f.now += timedelta(minutes=10)
    f.pu.step()
    assert len(f.sessions) == 2


def test_a_use_left_open_is_ended(tmp_path):
    f = Factory(tmp_path).candidate()
    f("power_user", "use_started", use=1, version="0.7.11", build=1)
    f.pu.step()
    assert f.ledger.world.is_(M.USE, 1, "ended") and f.sessions == []
    f.now += timedelta(hours=3)
    f.pu.step()  # that one was cut off: it is no use of the build, so here is one
    assert len(f.sessions) == 1


def test_a_session_that_fails_is_no_use_of_the_build_and_is_tried_again_sooner(tmp_path):
    f = Factory(tmp_path).candidate()
    f.exit = 1  # out of quota, say
    f.pu.step()
    assert f.ledger.world.uses[1].void and "uses" in f.ledger.world.hold_reasons("0.7.11")
    f.pu.step()
    assert len(f.sessions) == 1  # not at once
    f.exit = 0
    f.now += timedelta(minutes=31)
    f.pu.step()
    assert len(f.sessions) == 2 and "uses" not in f.ledger.refresh().hold_reasons("0.7.11")


def test_a_session_that_crashes_still_ends_its_use(tmp_path):
    def boom(f):
        raise RuntimeError("harness fell over")
    f = Factory(tmp_path, during=boom).candidate()
    try:
        f.pu.step()
    except RuntimeError:
        pass
    assert f.ledger.world.is_(M.USE, 1, "ended")


def test_no_uses_required_no_sessions(tmp_path):
    f = Factory(tmp_path, uses_required=0).candidate()
    f.pu.step()
    assert f.sessions == []


def test_a_use_can_be_voided_afterwards_and_the_build_is_used_again(tmp_path):
    import pytest
    from factory.ledger import Refused
    f = Factory(tmp_path).candidate()
    f.pu.step()
    f("human", "use_voided", use=1, note="it exited at once on the plan's limit")
    assert "uses" in f.ledger.world.hold_reasons("0.7.11")
    with pytest.raises(Refused, match="already void"):
        f("human", "use_voided", use=1, note="again")
    f.now += timedelta(minutes=31)
    f.pu.step()
    assert len(f.sessions) == 2 and "uses" not in f.ledger.refresh().hold_reasons("0.7.11")


def test_a_checkout_that_fails_opens_no_use(tmp_path):
    import subprocess
    f = Factory(tmp_path).candidate()
    def broken(sha):
        raise subprocess.CalledProcessError(1, "git fetch")
    f.pu.prepare = broken
    try:
        f.pu.step()
    except subprocess.CalledProcessError:
        pass
    assert f.ledger.world.uses == {} and f.sessions == []
