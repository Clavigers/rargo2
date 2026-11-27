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
    def __init__(self, tmp, uses_required=1, during=None):
        self.now = T0
        self.ledger = Ledger(tmp / "log.jsonl", clock=lambda: self.now)
        cfg = copy.deepcopy(CONFIG)
        cfg["release"] = {"freeze_every_days": 7, "uses_required": uses_required, "soak_minutes": 0}
        self("human", "config_applied", config=cfg)
        self.sessions, self.said, self.during = [], [], during
        self.pu = PowerUser(self.ledger, tmp / "use", "prompt", clock=lambda: self.now,
                            settle=0, run=self.session, say=self.said.append)

    def __call__(self, actor, event, **fields):
        self.now += timedelta(minutes=1)
        return self.ledger.submit(actor, json.dumps({"event": event, **fields}))

    def session(self, use, version, build, sha):
        self.sessions.append((use, version, build, sha))
        if self.during:
            self.during(self)
        return 0

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


def test_a_new_build_is_used_but_not_before_the_gap(tmp_path):
    f = Factory(tmp_path).candidate()
    f.pu.step()
    f("pm", "built", version="0.7.11", build=2, sha="b" * 40)
    f.pu.step()
    assert len(f.sessions) == 1
    f.now += timedelta(hours=3)
    f.pu.step()
    assert f.sessions[-1] == (2, "0.7.11", 2, "b" * 40)


def test_a_use_left_open_is_ended(tmp_path):
    f = Factory(tmp_path).candidate()
    f("power_user", "use_started", use=1, version="0.7.11", build=1)
    f.pu.step()
    assert f.ledger.world.is_(M.USE, 1, "ended") and f.sessions == []
    f.now += timedelta(hours=3)
    f.pu.step()  # that one ended without a session's worth of use, but it counts as the log says
    assert f.sessions == []


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
