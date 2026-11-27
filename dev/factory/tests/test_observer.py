import json
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest

from factory import machines as M
from factory import model as m
from factory.dictator import Dictator
from factory.github import GitHubError
from factory.ledger import Ledger
from factory.machines import IssueS
from factory.observer import Observer, SIGN_IN_STEP

OWNER = "sadosystems"
SHA, SHA2, MAIN = "a" * 40, "b" * 40, "c" * 40
CONFIG = {
    "version": 1,
    "plans": [{"name": "max", "hours_per_week": 168.0}],
    "agents": [{"type": "claude-code", "count": 2, "model": "claude-opus-5-5", "plan": "max"}],
    "reviewers": ["placeholder"],
    "dictator": {"tick_minutes": 20, "preempt_minutes": 60},
    "release": {"freeze_every_days": 7, "uses_required": 1, "soak_minutes": 120},
}
A1, A2 = "claude-code-1", "claude-code-2"


class FakeGitHub:
    def __init__(self):
        self.issue_list: dict[int, dict] = {}
        self.pulls: dict[int, dict] = {}
        self.runs: dict[str, list] = {}
        self.jobs: dict[int, dict] = {}
        self.rerun: list[str] = []
        self.posted: list[tuple[int, str]] = []
        self.closed: list[int] = []
        self.merge_error: str | None = None

    def issue(self, n, labels=("enhancement",), body="", by=OWNER, state="open",
              closed_by=None, state_reason=None, assignees=()):
        self.issue_list[n] = {"number": n, "labels": [{"name": x} for x in labels],
                              "assignees": [{"login": a} for a in assignees],
                              "body": body, "user": {"login": by}, "state": state,
                              "closed_by": {"login": closed_by} if closed_by else None,
                              "state_reason": state_reason}

    def pull_request(self, n, head):
        self.pulls[n] = {"number": n, "state": "open", "head": {"sha": head},
                         "merged_at": None, "merged_by": None, "merge_commit_sha": None,
                         "mergeable_state": "clean"}

    def ci(self, sha, conclusion, status="completed", app="github-actions", failed_steps=()):
        job_id = len(self.jobs) + 1
        self.jobs[job_id] = {"conclusion": conclusion, "steps": [
            {"name": name, "conclusion": "failure"} for name in failed_steps
        ]}
        self.runs.setdefault(sha, []).append(
            {"id": job_id, "app": {"slug": app}, "status": status, "conclusion": conclusion})

    def job(self, check_run_id):
        return self.jobs[check_run_id]

    def rerun_failed(self, sha):
        self.rerun.append(sha)
        return 1

    # the interface the observer uses
    def issues(self, since):
        return [i for i in self.issue_list.values() if since or i["state"] == "open"]

    def pull(self, n):
        return self.pulls[n]

    def get_issue(self, n):
        return self.issue_list[n]

    def open_pulls(self):
        return [p for p in self.pulls.values() if p["state"] == "open"]

    def check_runs(self, sha):
        return self.runs.get(sha, [])

    def merge(self, pr, sha):
        p = self.pulls[pr]
        if p["head"]["sha"] != sha:
            raise GitHubError("head moved")
        if self.merge_error:
            p["mergeable_state"] = self.merge_error
            raise GitHubError("not mergeable")
        p.update(state="closed", merged_at="now", merged_by={"login": OWNER},
                 merge_commit_sha=MAIN)

    def comment(self, n, body):
        self.posted.append((n, body))

    def comments(self, n):
        return [b for m, b in self.posted if m == n]

    def close(self, n, reason, body):
        self.closed.append(n)


class Clock:
    def __init__(self):
        self.t = datetime(2026, 11, 25, tzinfo=timezone.utc)

    def __call__(self):
        self.t += timedelta(seconds=10)
        return self.t


@pytest.fixture
def rig(tmp_path):
    clock = Clock()
    led = Ledger(tmp_path / "log.jsonl", clock)
    led.submit("human", json.dumps({"event": "config_applied", "config": CONFIG}))
    for a in (A1, A2):
        led.submit("watchdog", json.dumps({"event": "session_started", "agent": a}))
    gh = FakeGitHub()
    said = []
    obs = Observer(led, gh, OWNER, say=said.append)
    return type("Rig", (), dict(led=led, gh=gh, obs=obs, said=said, clock=clock))


def submit(rig, actor, event, **fields):
    return rig.led.submit(actor, json.dumps({"event": event, **fields}))


def lines(rig):
    return [json.loads(x) for x in open(rig.led.path)]


def test_issues_are_imported_in_dependency_order(rig):
    rig.gh.issue(1, labels=["initiative"])
    rig.gh.issue(5, body="Part of #1\nDepends on #7\n")
    rig.gh.issue(7, body="Part of #1")
    rig.gh.issue(8, labels=[])  # no type
    rig.gh.issue(9, by="stranger")  # not ours
    rig.obs.step()
    w = rig.led.world
    assert sorted(w.issues) == [1, 5, 7]
    assert w.issues[5].deps == {7} and w.issues[5].parent == 1
    assert w.state(M.ISSUE, 5) == IssueS.blocked
    assert [n for n, _ in rig.gh.posted] == [8]
    rig.obs.step()
    fresh = Observer(rig.led, rig.gh, OWNER, say=rig.said.append)  # a restart
    fresh.step()
    assert [n for n, _ in rig.gh.posted] == [8]  # asked once, ever


def test_an_agents_issue_is_recorded_as_the_agents(rig):
    rig.gh.issue(1)
    rig.obs.step()
    submit(rig, A1, "taken", issue=1)
    rig.gh.issue(2, labels=["bug"], body=f"found it\n<!-- factory-actor: {A1} -->")
    rig.obs.step()
    assert rig.led.world.issues[2].found == ("work", 1)
    assert lines(rig)[-1]["actor"] == A1


def pushed(rig, sha=SHA):
    rig.gh.issue(1)
    rig.obs.step()
    submit(rig, A1, "taken", issue=1)
    submit(rig, A1, "push", issue=1, pr=10, push=1, sha=sha)
    rig.gh.pull_request(10, sha)


def test_green_and_approved_merges(rig):
    pushed(rig)
    rig.obs.step()  # CI still running
    assert rig.led.world.is_(M.CI, "10/1", "pending")
    rig.gh.ci(SHA, "success")
    rig.gh.ci(SHA, "failure", app="coderabbit")  # not CI
    rig.obs.step()
    # the placeholder approved on the first step, while CI was still running
    assert [(d["event"], d["actor"]) for d in lines(rig)[-3:]] == [
        ("review_done", "reviewer"), ("ci_done", "ci"), ("merged", "merger")]
    assert lines(rig)[-2]["result"] == "pass" and lines(rig)[-1]["sha"] == MAIN
    w = rig.led.world
    assert w.is_(M.PR, 10, "merged") and w.is_(M.ISSUE, 1, IssueS.closed)


def test_red_ci_sends_it_back(rig):
    pushed(rig)
    rig.gh.ci(SHA, "failure", failed_steps=["Build"])
    rig.obs.step()
    w = rig.led.world
    assert w.is_(M.CI, "10/1", "fail") and w.needs_work(1)
    assert rig.gh.rerun == []


def test_sign_in_step_matches_workflow():
    workflow = Path(__file__).resolve().parents[3] / ".github/workflows/ci.yml"
    step_names = [line.strip().removeprefix("- name: ") for line in workflow.read_text().splitlines()
                  if line.lstrip().startswith("- name: ")]
    assert SIGN_IN_STEP in step_names


def test_sign_in_failure_keeps_ci_pending_and_throttles_reruns(rig, monkeypatch):
    now = 0
    monkeypatch.setattr("factory.observer.monotonic", lambda: now)
    pushed(rig)
    rig.gh.ci(SHA, "failure", failed_steps=[SIGN_IN_STEP])
    rig.gh.ci(SHA, "success")
    rig.obs.step()
    assert rig.led.world.is_(M.CI, "10/1", "pending")
    assert rig.gh.rerun == [SHA]
    assert any("keeping CI pending" in message for message in rig.said)

    now = 599
    rig.obs.step()
    assert rig.gh.rerun == [SHA]
    now = 600
    rig.obs.step()
    assert rig.gh.rerun == [SHA, SHA]
    assert not any(line["event"] == "ci_done" for line in lines(rig))

    rig.gh.runs[SHA].clear()
    rig.gh.ci(SHA, "success")
    rig.obs.step()
    assert rig.led.world.is_(M.CI, "10/1", "pass")


def test_sign_in_and_another_job_failure_sends_it_back(rig):
    pushed(rig)
    rig.gh.ci(SHA, "failure", failed_steps=[SIGN_IN_STEP])
    rig.gh.ci(SHA, "failure", failed_steps=["Test"])
    rig.obs.step()
    w = rig.led.world
    assert w.is_(M.CI, "10/1", "fail") and w.needs_work(1)
    assert rig.gh.rerun == []


def test_a_foreign_push_sends_the_issue_back_to_an_agent(rig):
    pushed(rig)
    rig.gh.pulls[10]["head"]["sha"] = SHA2  # someone outside pushed to the branch
    rig.gh.ci(SHA2, "success")
    rig.obs.step()
    w = rig.led.world
    assert lines(rig)[-1]["event"] == "diverged" and lines(rig)[-1]["reason"] == "foreign_push"
    assert w.is_(M.CI, "10/1", "void") and w.needs_work(1)  # never merged unchecked
    rig.obs.step()  # said once, not again
    assert sum(d["event"] == "diverged" for d in lines(rig)) == 1


def test_a_pr_closed_outside_the_factory_is_abandoned(rig):
    pushed(rig)
    rig.gh.pulls[10]["state"] = "closed"
    rig.obs.step()
    w = rig.led.world
    assert w.is_(M.PR, 10, "abandoned") and w.issues[1].pr is None and w.waiting(1)


def test_an_outside_pr_that_closes_an_issue_locks_it(rig):
    rig.gh.issue(1)
    rig.obs.step()
    rig.gh.pulls[50] = {"number": 50, "state": "open", "user": {"login": "stranger-bot"},
                        "title": "Fix the thing", "body": "Closes #1", "head": {"sha": SHA2}}
    rig.obs.step()
    assert rig.led.world.issues[1].parked
    assert lines(rig)[-1] == {**lines(rig)[-1], "event": "parked", "actor": "outside", "reason": "lock"}
    rig.gh.pulls[50]["state"] = "closed"  # they gave up: the issue is free again
    rig.obs.step()
    assert not rig.led.world.issues[1].parked


def test_a_merge_conflict_is_recorded(rig):
    pushed(rig)
    rig.gh.ci(SHA, "success")
    rig.gh.merge_error = "dirty"
    rig.obs.step()
    assert lines(rig)[-1]["event"] == "merge_failed" and lines(rig)[-1]["reason"] == "conflict"
    assert rig.led.world.needs_work(1)


def test_a_conflicting_push_gets_no_ci_and_needs_work(rig):
    pushed(rig)
    rig.gh.pulls[10]["mergeable_state"] = "unknown"  # GitHub has not computed it yet
    rig.obs.step()
    assert rig.led.world.is_(M.CI, "10/1", "pending")  # keeps waiting
    rig.gh.pulls[10]["mergeable_state"] = "dirty"  # and so no CI run starts
    rig.obs.step()
    assert lines(rig)[-1] == {**lines(rig)[-1], "event": "merge_failed", "actor": "merger",
                              "pr": 10, "push": 1, "reason": "conflict"}
    w = rig.led.world
    assert w.is_(M.CI, "10/1", "void") and w.needs_work(1) and w.issues[1].pr == 10
    rig.obs.step()  # recorded once
    assert sum(d["event"] == "merge_failed" for d in lines(rig)) == 1


def test_a_conflict_with_ci_running_waits_for_ci(rig):
    pushed(rig)
    rig.gh.ci(SHA, None, status="in_progress")
    rig.gh.pulls[10]["mergeable_state"] = "dirty"  # main moved after CI started
    rig.obs.step()
    assert rig.led.world.is_(M.CI, "10/1", "pending")
    assert not any(d["event"] == "merge_failed" for d in lines(rig))


def test_merges_and_closes_on_github_are_followed(rig):
    pushed(rig)
    rig.gh.pulls[10].update(state="closed", merged_at="now", merged_by={"login": "someone"},
                            merge_commit_sha=MAIN)
    rig.gh.issue(2)
    rig.obs.step()
    merged = next(d for d in lines(rig) if d["event"] == "merged")
    assert merged["actor"] == "outside" and merged["sha"] == MAIN
    assert rig.led.world.is_(M.ISSUE, 1, IssueS.closed)
    submit(rig, A2, "taken", issue=2)
    rig.gh.issue(2, state="closed", closed_by=OWNER, state_reason="not_planned")
    rig.obs.step()
    w = rig.led.world
    assert w.is_(M.ISSUE, 2, IssueS.closed) and A2 not in w.holding
    assert lines(rig)[-1]["actor"] == "human" and lines(rig)[-1]["reason"] == "wontfix"


# ---------------------------------------------------------------- dictator


def test_the_dictator_takes_back_closes_and_plans(rig, tmp_path):
    rig.gh.issue(1, labels=["initiative"])
    rig.gh.issue(2, body="Part of #1")
    rig.gh.issue(3, labels=["initiative"])
    rig.gh.issue(4)
    rig.obs.step()
    submit(rig, A1, "taken", issue=4)
    submit(rig, "watchdog", "session_ended", agent=A1, reason="crashed")
    planned = []
    fm = Dictator(rig.led, rig.gh, tmp_path, "plan {initiative}", clock=rig.clock,
                 run=lambda n: planned.append(n) or 0, say=rig.said.append)
    fm.tick()
    assert rig.led.world.is_(M.ISSUE, 4, IssueS.ready)  # stale, taken back
    assert planned == [3]  # #1 has a child already
    rig.gh.issue(2, state="closed", closed_by=OWNER, state_reason="completed")
    rig.obs.step()
    fm.tick()
    assert rig.led.world.is_(M.INITIATIVE, 1, "closed") and rig.gh.closed == [1]
    assert planned == [3]  # not again within a day


def test_the_dictator_preempts_for_a_waiting_bug(rig, tmp_path):
    rig.gh.issue(1)
    rig.gh.issue(2)
    rig.obs.step()
    submit(rig, A1, "taken", issue=1)
    submit(rig, A2, "taken", issue=2)
    rig.gh.issue(3, labels=["bug"], body=f"<!-- factory-actor: {A1} -->")
    rig.obs.step()
    fm = Dictator(rig.led, rig.gh, tmp_path, "", clock=rig.clock, run=lambda n: 0,
                 say=rig.said.append)
    fm.tick()
    assert set(rig.led.world.holding) == {A1, A2}  # the bug has not waited long
    rig.clock.t += timedelta(minutes=61)
    fm.tick()
    assert rig.led.world.holding == {A1: 1}  # #2 was taken last


def test_the_guard_label_parks(rig, tmp_path):
    rig.gh.issue(1, labels=["initiative", "guard"])
    rig.gh.issue(2, labels=["enhancement", "guard"])
    rig.obs.step()
    w = rig.led.world
    assert w.issues[1].parked and w.state(M.ISSUE, 2) == IssueS.parked
    planned = []
    Dictator(rig.led, rig.gh, tmp_path, "", clock=rig.clock,
            run=lambda n: planned.append(n) or 0, say=rig.said.append).tick()
    assert planned == []  # a parked initiative is not planned
    rig.gh.issue(2, labels=["enhancement"])  # the owner lifts the hold
    rig.obs.step()
    assert rig.led.world.state(M.ISSUE, 2) == IssueS.ready


def test_wip_labels_are_outside_locks(rig):
    rig.gh.issue(1, labels=["bug", "wip-claude"])
    rig.obs.step()
    assert rig.led.world.state(M.ISSUE, 1) == IssueS.parked
    assert lines(rig)[-1]["actor"] == "outside" and lines(rig)[-1]["reason"] == "lock"
    rig.gh.issue(1, labels=["bug"])
    rig.obs.step()
    assert rig.led.world.state(M.ISSUE, 1) == IssueS.ready


def test_since_is_utc_whatever_the_machine_zone(monkeypatch):
    """The factory machine runs on CET; GitHub's since is UTC."""
    from factory.github import GitHub
    asked = []
    gh = GitHub("o/r")
    monkeypatch.setattr(gh, "_api", lambda path, paginate=False: asked.append(path) or [])
    cet = timezone(timedelta(hours=1))
    gh.issues(datetime(2026, 11, 26, 7, 30, tzinfo=cet))
    assert "since=2026-11-26T06:30:00Z" in asked[0]


def test_assigned_to_the_owner_is_the_owners(rig):
    rig.gh.issue(1, assignees=[OWNER])
    rig.gh.issue(2, assignees=["someone"])
    rig.obs.step()
    w = rig.led.world
    assert w.state(M.ISSUE, 1) == IssueS.parked and w.state(M.ISSUE, 2) == IssueS.ready


def test_the_dictator_closes_on_github_what_an_agent_closed(rig, tmp_path):
    rig.gh.issue(1)
    rig.gh.issue(2)
    rig.obs.step()
    submit(rig, A1, "taken", issue=1)
    submit(rig, A1, "closed", issue=1, reason="completed", note="it holds")
    submit(rig, A2, "taken", issue=2)
    submit(rig, A2, "closed", issue=2, reason="completed", note="it holds")
    rig.gh.issue(2, state="closed", closed_by={"login": "kevkev"}, state_reason="completed")
    fm = Dictator(rig.led, rig.gh, tmp_path, "", clock=rig.clock, run=lambda n: 0,
                 say=rig.said.append)
    fm.tick()
    fm.tick()
    assert rig.gh.closed == [1]  # #2 the agent closed itself; each once
    rig.obs.step()
    assert rig.led.world.issues[2].closer == A2  # the observer does not record it again



def with_review_mux(rig, verdicts):
    """The observer with a review-mux reviewer that answers from verdicts."""
    from factory.observer import ReviewerStuck

    def reviewer(gh, pr, sha):
        v = verdicts.get(sha)
        if isinstance(v, Exception):
            raise v
        return v
    submit(rig, "human", "config_applied", config=dict(CONFIG, version=2, reviewers=["review-mux"]))
    rig.obs = Observer(rig.led, rig.gh, OWNER, say=rig.said.append,
                       reviewers={"review-mux": reviewer})
    return ReviewerStuck


def test_review_mux_findings_send_it_back_with_the_link(rig):
    verdicts = {}
    with_review_mux(rig, verdicts)
    pushed(rig)
    rig.gh.ci(SHA, "success")
    rig.obs.step()
    assert rig.led.world.is_(M.REVIEW, "10/1/review-mux", "pending")
    verdicts[SHA] = (m.ReviewResult.changes, "1 finding from coderabbit: https://x/10#c1")
    rig.obs.step()
    last = lines(rig)[-1]
    assert (last["event"], last["result"], last["note"]) == (
        "review_done", "changes", "1 finding from coderabbit: https://x/10#c1")
    assert rig.led.world.needs_work(1)


def test_review_mux_approval_merges(rig):
    with_review_mux(rig, {SHA: (m.ReviewResult.approved, None)})
    pushed(rig)
    rig.gh.ci(SHA, "success")
    rig.obs.step()
    assert rig.led.world.is_(M.PR, 10, "merged")


def test_a_stuck_review_waits_and_says_so_once(rig):
    verdicts = {}
    stuck = with_review_mux(rig, verdicts)
    verdicts[SHA] = stuck("review-mux could not review aaaa: no bot gave a review")
    pushed(rig)
    rig.gh.ci(SHA, "success")
    rig.obs.step()
    rig.obs.step()
    assert rig.led.world.is_(M.REVIEW, "10/1/review-mux", "pending")
    assert sum("could not review" in x for x in rig.said) == 1


def test_ellipsis_review_of_the_pushed_commit_is_the_verdict():
    from factory.observer import ellipsis
    from factory import model as m

    class G:
        def __init__(self, rows):
            self.rows = rows

        def reviews(self, pr):
            return self.rows

        def pull(self, pr):
            return {"base": {"ref": "main"}}

        def change(self, base, sha):
            return [f"+{sha}"]
    bot = {"login": "ellipsis-dev[bot]"}
    assert ellipsis(G([]), 1, "aa") is None
    assert ellipsis(G([{"user": bot, "commit_id": "old", "state": "APPROVED"}]), 1, "aa") is None
    assert ellipsis(G([{"user": {"login": "x"}, "commit_id": "aa", "state": "APPROVED"}]), 1, "aa") is None
    assert ellipsis(G([{"user": bot, "commit_id": "aa", "state": "APPROVED"}]), 1, "aa") == (m.ReviewResult.approved, None)
    verdict, note = ellipsis(G([{"user": bot, "commit_id": "aa", "state": "COMMENTED", "html_url": "u"}]), 1, "aa")
    assert verdict is m.ReviewResult.changes and "u" in note


def test_ellipsis_review_carries_to_a_push_that_makes_the_same_change():
    """Ellipsis does not review a push that only merges main in or rebases:
    the factory waited for ever for it (13 pull requests, 2026-11-27)."""
    from factory.observer import ellipsis
    from factory import model as m

    class G:
        def __init__(self, rows, changes):
            self.rows, self.changes = rows, changes

        def reviews(self, pr):
            return self.rows

        def pull(self, pr):
            return {"base": {"ref": "main"}}

        def change(self, base, sha):
            assert base == "main"
            return self.changes[sha]
    bot = {"login": "ellipsis-dev[bot]"}
    change = ["+++ b/a.rs", "+x"]
    ok = [{"user": bot, "commit_id": "c1", "state": "APPROVED"}]
    assert ellipsis(G(ok, {"c1": change, "m1": list(change)}), 1, "m1") == (m.ReviewResult.approved, None)
    assert ellipsis(G(ok, {"c1": change, "m2": change + ["+y"]}), 1, "m2") is None
    assert ellipsis(G(ok, {"c1": [], "m3": []}), 1, "m3") is None
    asked = [{"user": bot, "commit_id": "c2", "state": "COMMENTED", "html_url": "u"}]
    verdict, note = ellipsis(G(asked, {"c2": change, "m4": change}), 1, "m4")
    assert verdict is m.ReviewResult.changes and "u" in note
    # only its last review carries
    two = ok + [{"user": bot, "commit_id": "c3", "state": "COMMENTED", "html_url": "u"}]
    assert ellipsis(G(two, {"c1": change, "c3": change + ["+y"], "m5": change}), 1, "m5") is None


class Unreviewed:
    """A pull request Ellipsis has not reviewed, on a clock the test moves."""
    BOT = "ellipsis-dev[bot]"

    def __init__(self, monkeypatch):
        from factory import observer
        self.t = datetime(2026, 11, 27, 6, tzinfo=timezone.utc)
        monkeypatch.setattr(observer, "now", lambda: self.t)
        monkeypatch.setattr(observer, "_first_seen", {})
        monkeypatch.setattr(observer, "_asked", {})
        self.said: list[dict] = []
        self.posted: list[tuple[int, str, str]] = []

    def later(self, minutes):
        self.t += timedelta(minutes=minutes)

    def say(self, body):
        self.said.append({"user": {"login": self.BOT}, "body": body, "html_url": "https://x/10#c9",
                          "created_at": self.t.strftime("%Y-%m-%dT%H:%M:%SZ")})

    def reviews(self, pr):
        return []

    def issue_comments(self, pr):
        return self.said

    def post(self, pr, body, as_=None):
        self.posted.append((pr, body, as_))
        return 1


def test_ellipsis_with_no_review_is_asked_once_and_its_answer_is_the_verdict(monkeypatch):
    from factory.observer import ellipsis
    trigger = json.loads((Path(__file__).parent.parent / "deploy" / "bots.json").read_text())["ellipsis"]
    g = Unreviewed(monkeypatch)
    assert ellipsis(g, 10, "aa") is None
    g.later(30)
    assert ellipsis(g, 10, "aa") is None and g.posted == []  # not yet past the wait
    g.later(1)
    assert ellipsis(g, 10, "aa") is None
    g.later(5)
    assert ellipsis(g, 10, "aa") is None
    assert g.posted == [(10, trigger["trigger"]["comment"], trigger["trigger_as"])]  # once
    g.later(1)
    g.say("I didn't find any bugs in this PR. 8 lines, $0.20")
    assert ellipsis(g, 10, "aa") == (m.ReviewResult.approved, None)

    g = Unreviewed(monkeypatch)
    ellipsis(g, 10, "bb")
    g.later(31)
    ellipsis(g, 10, "bb")
    g.later(1)
    g.say("The loop in a.py skips its last item. $0.31")
    verdict, note = ellipsis(g, 10, "bb")
    assert verdict is m.ReviewResult.changes and "https://x/10#c9" in note


def test_ellipsis_with_no_answer_when_asked_sends_the_push_back(rig, monkeypatch):
    g = Unreviewed(monkeypatch)
    for name in ("reviews", "issue_comments", "post"):
        setattr(rig.gh, name, getattr(g, name))
    submit(rig, "human", "config_applied", config=dict(CONFIG, version=2, reviewers=["ellipsis"]))
    pushed(rig)
    rig.gh.ci(SHA, "success")
    rig.obs.step()
    g.later(31)
    rig.obs.step()
    assert len(g.posted) == 1
    g.later(30)
    rig.obs.step()
    assert rig.led.world.is_(M.REVIEW, "10/1/ellipsis", "pending")  # still within the second wait
    g.later(1)
    rig.obs.step()
    last = lines(rig)[-1]
    assert (last["event"], last["by"], last["result"]) == ("review_done", "ellipsis", "changes")
    assert "nor answered being asked" in last["note"]
    assert rig.led.world.needs_work(1)
    assert len(g.posted) == 1 and not any("stuck" in x for x in rig.said)


def test_the_dictator_orchestrates_inside_a_tick_of_its_own(rig, tmp_path):
    import copy
    cfg = copy.deepcopy(CONFIG) | {"version": 2}
    cfg["dictator"] |= {"orchestrate_minutes": 180, "model": "claude-fable-5-2"}
    submit(rig, "human", "config_applied", config=cfg)
    rig.gh.issue(1)
    rig.gh.issue(2)
    rig.obs.step()
    ran = []

    def session(what, prompt=None):  # the session decides, as the dictator
        ran.append((what, prompt))
        if len(ran) == 1:
            submit(rig, "dictator", "depends", issue=2, on=1)
        return 0
    fm = Dictator(rig.led, rig.gh, tmp_path, "", clock=rig.clock, run=session, say=rig.said.append,
                 orchestrate_prompt="notes at {notes}, state at {state}", state=tmp_path / "state")
    fm.tick()
    assert ran == [("orchestrate", f"notes at {tmp_path / 'orchestration.md'}, state at {tmp_path / 'state'}")]
    assert 1 in rig.led.world.issues[2].deps and not rig.led.world.is_(M.DICTATOR, "", "ticking")
    fm.tick()  # not again until orchestrate_minutes have passed
    assert len(ran) == 1
    rig.clock.t += timedelta(minutes=181)
    fm.tick()
    assert len(ran) == 2
