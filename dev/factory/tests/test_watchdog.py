import json
import os
import pathlib
import subprocess
import sys
from datetime import datetime, timedelta, timezone

import pytest

from factory import machines as M
from factory.ledger import Ledger
from factory.machines import AgentS
from factory.watchdog import Watchdog

ROOT = pathlib.Path(__file__).parent.parent
FAKE = f"""#!{sys.executable}
import json, os, subprocess, sys, time
mode = os.environ["FAKE_MODE"]
f = [sys.executable, "-m", "factory"]
if mode == "work":
    ready = json.loads(subprocess.check_output(f + ["ready"]))
    subprocess.run(f + ["submit", json.dumps({{"event": "taken", "issue": ready[0]["issue"]}})],
                   check=True)
elif mode == "quota":
    print("You have hit your usage limit. Try again at 5pm.")
    sys.exit(1)
elif mode == "crash":
    sys.exit(1)
elif mode == "sleep":
    time.sleep(60)
"""
CONFIG = {
    "version": 1,
    "plans": [{"name": "max", "hours_per_week": 168.0}],
    "agents": [{"type": "claude-code", "count": 1, "model": "claude-opus-5-5", "plan": "max"}],
    "reviewers": ["placeholder"],
    "foreman": {"tick_minutes": 20, "preempt_minutes": None},
    "release": {"freeze_every_days": 7, "uses_required": 1, "soak_minutes": 120},
}
A = "claude-code-1"


class Clock:
    def __init__(self):
        self.t = datetime(2026, 11, 25, tzinfo=timezone.utc)

    def __call__(self):
        return self.t

    def advance(self, **kw):
        self.t += timedelta(**kw)


@pytest.fixture
def rig(tmp_path, monkeypatch):
    repo = tmp_path / "repo"
    repo.mkdir()
    for cmd in (["init", "-q"], ["commit", "-q", "--allow-empty", "-m", "root"]):
        subprocess.run(["git", "-C", str(repo), "-c", "user.name=t", "-c", "user.email=t@t",
                        *cmd], check=True)
    fake = tmp_path / "fake-claude"
    fake.write_text(FAKE)
    fake.chmod(0o755)
    monkeypatch.setenv("FACTORY_CLAUDE_CODE", str(fake))
    monkeypatch.setenv("PYTHONPATH", str(ROOT))
    clock = Clock()
    log = tmp_path / "log.jsonl"
    led = Ledger(log, clock)
    led.submit("human", json.dumps({"event": "config_applied", "config": CONFIG}))
    led.submit("human", json.dumps({"event": "filed", "issue": 1, "kind": "enhancement"}))
    said = []
    wds = []

    def watchdog():
        wd = Watchdog(Ledger(log, clock), repo, tmp_path / "state", "you are {agent}",
                      base="HEAD", clock=clock, say=said.append)
        wds.append(wd)
        return wd

    yield type("Rig", (), dict(log=log, clock=clock, watchdog=watchdog, said=said,
                               monkeypatch=monkeypatch))
    for wd in wds:
        for s in wd.sessions.values():
            try:
                os.kill(s.pid, 9)
            except ProcessLookupError:
                pass


def events(log):
    return [(d["event"], d.get("reason")) for d in map(json.loads, log.read_text().splitlines())
            if d["actor"] in ("watchdog", A)]


def finish(wd):
    for s in wd.sessions.values():
        s.proc.wait(30)


def test_a_session_works_and_the_watchdog_starts_the_next(rig):
    rig.monkeypatch.setenv("FAKE_MODE", "work")
    wd = rig.watchdog()
    wd.step()
    assert (rig.log.parent / "state" / "worktrees" / A / ".git").exists()
    finish(wd)
    wd.step()
    assert events(rig.log) == [("session_started", None), ("taken", None),
                               ("session_ended", "finished"), ("session_started", None)]
    assert wd.ledger.world.holding == {A: 1}


def test_quota_stops_an_agent_until_it_is_worth_trying_again(rig):
    rig.monkeypatch.setenv("FAKE_MODE", "quota")
    wd = rig.watchdog()
    wd.step()
    finish(wd)
    wd.step()
    assert events(rig.log)[1:] == [("session_ended", "quota"), ("quota_out", None)]
    assert A not in wd.sessions
    rig.clock.advance(minutes=31)
    rig.monkeypatch.setenv("FAKE_MODE", "sleep")
    wd.step()
    assert events(rig.log)[3:] == [("quota_back", None), ("session_started", None)]


def test_crashes_back_off(rig):
    rig.monkeypatch.setenv("FAKE_MODE", "crash")
    wd = rig.watchdog()
    wd.step()
    finish(wd)
    wd.step()
    assert events(rig.log)[-1] == ("session_ended", "crashed")
    assert A not in wd.sessions  # backing off for a minute
    rig.clock.advance(minutes=1)
    wd.step()
    assert A in wd.sessions
    finish(wd)
    wd.step()
    rig.clock.advance(minutes=1)
    wd.step()
    assert A not in wd.sessions  # the second crash in a row waits two minutes


def test_a_restarted_watchdog_adopts_running_sessions(rig):
    rig.monkeypatch.setenv("FAKE_MODE", "sleep")
    first = rig.watchdog()
    first.step()
    pid = first.sessions[A].pid
    second = rig.watchdog()  # the first one died
    second.step()
    assert second.sessions[A].pid == pid
    assert events(rig.log) == [("session_started", None)]
    os.kill(pid, 9)  # the wrapper itself: no exit code is left
    first.sessions[A].proc.wait()  # reap it, as init would
    second.step()
    assert events(rig.log)[1] == ("session_ended", "crashed")


def test_an_adopted_session_reports_how_it_ended(rig):
    rig.monkeypatch.setenv("FAKE_MODE", "quota")
    first = rig.watchdog()
    first.step()
    first.sessions[A].proc.wait()  # it ended while no watchdog watched
    second = rig.watchdog()
    second.step()
    assert events(rig.log)[1:] == [("session_ended", "quota"), ("quota_out", None)]


def test_a_lost_session_is_recorded(rig):
    rig.monkeypatch.setenv("FAKE_MODE", "sleep")
    first = rig.watchdog()
    first.step()
    os.kill(first.sessions[A].pid, 9)
    first.sessions[A].proc.wait()
    (rig.log.parent / "state" / "pids" / A).unlink()  # and its pid file is gone too
    second = rig.watchdog()
    rig.monkeypatch.setenv("FAKE_MODE", "crash")
    second.step()
    assert events(rig.log)[1] == ("session_ended", "crashed")
    assert "live in the log but has no process here" in " ".join(rig.said)


def test_no_harness_no_session(rig):
    rig.monkeypatch.setenv("FACTORY_CLAUDE_CODE", "")
    rig.monkeypatch.setenv("PATH", "/nonexistent")
    wd = rig.watchdog()
    wd.step()
    assert events(rig.log) == []
    assert wd.ledger.world.is_(M.AGENT, A, AgentS.off)


def test_no_work_no_session(rig):
    rig.monkeypatch.setenv("FAKE_MODE", "sleep")
    led = Ledger(rig.log, rig.clock)
    led.submit("human", json.dumps({"event": "closed", "issue": 1, "reason": "wontfix"}))
    wd = rig.watchdog()
    wd.step()
    assert events(rig.log) == [] and A not in wd.sessions
    led.submit("human", json.dumps({"event": "filed", "issue": 2, "kind": "enhancement"}))
    wd.step()
    assert A in wd.sessions
