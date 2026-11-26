"""The visualizer's simulation makes only legal moves, and the page's view
builds from the real log and from the simulation alike."""
import json
import pathlib

import pytest

pytest.importorskip("simpy")

from factory import model as m  # noqa: E402
from factory.world import World  # noqa: E402
from viz.simfactory import DAY, Sim  # noqa: E402
from viz.view import History, snapshot  # noqa: E402

GOLDEN = pathlib.Path(__file__).parent / "golden.jsonl"


@pytest.mark.parametrize("seed", [1, 2, 3])
def test_a_simulated_week_is_legal_and_does_work(seed):
    s = Sim(seed=seed)
    s.run_until(7 * DAY)  # World.apply refuses any illegal move
    events = [l["event"] for l in s.lines]
    assert events.count("merged") > 10 and "taken" in events
    # The lines are a log the real checker accepts from scratch.
    w = World()
    for l in s.lines:
        w.apply(m.parse(json.dumps(l)))
    assert w.seq == len(s.lines)


def test_extreme_parameters_stay_legal():
    s = Sim({"issues_per_day": 40, "quota_per_hour": 60, "give_up": 30, "blocked": 30,
             "crash": 20, "ci_fail": 60, "review_mux": 1, "review_changes": 60,
             "conflict": 30, "initiatives_per_week": 5, "codex": 3}, seed=7)
    s.run_until(3 * DAY)
    s.p.update(codex=0, grok=0, review_mux=0)
    s.poke()
    s.run_until(5 * DAY)
    assert any(l["event"] == "config_applied" for l in s.lines[1:])


def test_the_view_builds_from_the_real_log():
    w, lines, h = World(), [], History()
    for text in GOLDEN.read_text().splitlines():
        w.apply(m.parse(text))
        lines.append(json.loads(text))
        h.saw(lines[-1])
        h.sample(w)
    snap = snapshot(w, lines)
    json.dumps(snap)
    assert snap["seq"] == len(lines) and snap["agents"] and h.rows
