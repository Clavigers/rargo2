import json
import multiprocessing as mp
import os
import pathlib
import subprocess
import sys
from datetime import datetime, timedelta, timezone

import pytest

from factory.ledger import Alarm, Ledger, Refused
from factory.machines import IssueS
from factory import machines as M

ROOT = pathlib.Path(__file__).parent.parent
T0 = datetime(2026, 11, 25, tzinfo=timezone.utc)
N = 8
CONFIG = {
    "version": 1,
    "plans": [{"name": "max", "hours_per_week": 168.0}],
    "agents": [{"type": "claude-code", "count": N, "model": "claude-opus-5-5",
                "plan": "max"}],
    "reviewers": ["placeholder"],
    "foreman": {"tick_minutes": 20, "preempt_minutes": None},
    "release": {"freeze_every_days": 7, "uses_required": 1, "soak_minutes": 120},
}
AGENTS = [f"claude-code-{i}" for i in range(1, N + 1)]


class Clock:
    def __init__(self):
        self.t = T0

    def __call__(self):
        self.t += timedelta(seconds=30)
        return self.t


def body(event, **fields):
    return json.dumps({"event": event, **fields})


def started(path) -> Ledger:
    led = Ledger(path, Clock())
    led.submit("human", body("config_applied", config=CONFIG))
    for a in AGENTS:
        led.submit("watchdog", body("session_started", agent=a))
    led.submit("human", body("filed", issue=1, kind="enhancement"))
    return led


def check(path) -> subprocess.CompletedProcess:
    return subprocess.run([sys.executable, "-m", "factory", "check", str(path)],
                          capture_output=True, text=True, cwd=ROOT)


def test_the_ledger_stamps_and_the_log_checks(tmp_path):
    log = tmp_path / "log.jsonl"
    led = started(log)
    line = led.submit(AGENTS[0], body("taken", issue=1))
    assert line.seq == N + 3 and line.t == T0 + timedelta(seconds=30 * (N + 3))
    assert check(log).returncode == 0


def test_time_never_runs_backwards(tmp_path):
    led = started(tmp_path / "log.jsonl")
    last = led.world.t
    led.clock = lambda: T0  # a clock that jumped back
    assert led.submit(AGENTS[0], body("taken", issue=1)).t == last
    led.clock = lambda: datetime(2026, 11, 25)
    with pytest.raises(ValueError, match="aware"):
        led.submit(AGENTS[1], body("returned", issue=1, reason="gave_up"))


def test_a_refused_decision_writes_nothing(tmp_path):
    log = tmp_path / "log.jsonl"
    led = started(log)
    led.submit(AGENTS[0], body("taken", issue=1))
    size = log.stat().st_size
    with pytest.raises(Refused, match="not ready"):
        led.submit(AGENTS[1], body("taken", issue=1))
    assert log.stat().st_size == size
    led.submit(AGENTS[0], body("filed", issue=2, kind="bug"))  # the world was untouched


def test_an_impossible_observation_is_an_alarm(tmp_path):
    led = started(tmp_path / "log.jsonl")
    with pytest.raises(Alarm, match="no pr 9"):
        led.submit("ci", body("ci_done", pr=9, push=1, result="pass"))


def test_two_ledgers_on_one_log_see_each_other(tmp_path):
    log = tmp_path / "log.jsonl"
    a = started(log)
    b = Ledger(log, Clock())
    a.submit(AGENTS[0], body("taken", issue=1))
    with pytest.raises(Refused, match="held, not ready"):
        b.submit(AGENTS[1], body("taken", issue=1))
    b.submit(AGENTS[0], body("returned", issue=1, reason="gave_up"))
    assert a.refresh().state(M.ISSUE, 1) == IssueS.ready


def _race(path, agent, go, out):
    led = Ledger(path)
    go.wait()
    try:
        led.submit(agent, body("taken", issue=1))
        out.put(agent)
    except Refused:
        out.put(None)


def test_the_append_is_the_lock(tmp_path):
    log = tmp_path / "log.jsonl"
    started(log)
    ctx = mp.get_context("fork")
    go, out = ctx.Barrier(N), ctx.Queue()
    procs = [ctx.Process(target=_race, args=(log, a, go, out)) for a in AGENTS]
    for p in procs:
        p.start()
    for p in procs:
        p.join(10)
    winners = [w for w in (out.get(timeout=5) for _ in AGENTS) if w]
    assert len(winners) == 1
    world = Ledger(log).world
    assert world.issues[1].holder == winners[0]
    assert check(log).returncode == 0


def test_a_torn_tail_is_cut_by_the_next_writer(tmp_path):
    log = tmp_path / "log.jsonl"
    started(log)
    with open(log, "ab") as f:
        f.write(b'{"schema":1,"seq":')  # a writer died here, unacknowledged
    reader = Ledger(log, Clock())  # a reader ignores it
    assert reader.world.seq == N + 2
    assert check(log).returncode == 1  # the file as it stands is not a log
    reader.submit(AGENTS[0], body("taken", issue=1))
    assert check(log).returncode == 0


def test_the_submit_command(tmp_path, monkeypatch):
    log = tmp_path / "log.jsonl"
    started(log)
    monkeypatch.setenv("FACTORY_LOG", str(log))

    def submit(actor, *args):
        return subprocess.run([sys.executable, "-m", "factory", "submit", *args],
                              capture_output=True, text=True, cwd=ROOT,
                              env=os.environ | {"FACTORY_ACTOR": actor})

    size = log.stat().st_size
    assert submit(AGENTS[0], "--dry", body("taken", issue=1)).returncode == 0
    assert log.stat().st_size == size
    ok = submit(AGENTS[0], body("taken", issue=1))
    assert ok.returncode == 0 and json.loads(ok.stdout)["seq"] == N + 3
    assert submit(AGENTS[1], body("taken", issue=1)).returncode == 2
    assert submit("ci", body("ci_done", pr=9, push=1, result="pass")).returncode == 3
    assert submit(AGENTS[1], body("taken", issue="1")).returncode == 1
    assert submit(AGENTS[1], json.dumps({"event": "taken", "issue": 1, "seq": 4})).returncode == 1
