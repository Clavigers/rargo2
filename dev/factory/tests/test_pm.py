"""The project manager, against a real git repository and a real log."""
import copy
import json
import subprocess
from datetime import datetime, timedelta, timezone

import pytest

from factory import machines as M
from factory.ledger import Ledger
from factory.pm import Pm, bump
from test_world import C1, CONFIG

T0 = datetime(2026, 1, 5, 12, 0, tzinfo=timezone.utc)


def git(cwd, *args):
    return subprocess.run(["git", "-C", str(cwd), *args], check=True,
                          capture_output=True, text=True).stdout.strip()


class Checks:
    """GitHub's check runs: every commit passes unless told otherwise."""

    def __init__(self):
        self.result = "success"
        self.reruns = 0

    def rerun_failed(self, sha):
        self.reruns += 1
        return 1

    released, notes = (), None

    def release(self, tag):
        return {"tag_name": tag} if tag in self.released else None

    def set_release_notes(self, tag, path):
        self.notes = (tag, path.read_text())

    def check_runs(self, sha):
        if self.result is None:
            return []
        return [{"app": {"slug": "github-actions"}, "status": "completed", "conclusion": self.result}]


class Factory:
    def __init__(self, tmp, uses_required=0, soak=0, publish=("true",)):
        self.origin, self.dev, self.checkout = tmp / "origin.git", tmp / "dev", tmp / "release"
        subprocess.run(["git", "init", "-q", "--bare", "-b", "main", str(self.origin)], check=True)
        subprocess.run(["git", "clone", "-q", str(self.origin), str(self.dev)], check=True,
                       capture_output=True)
        for k, v in (("user.name", "t"), ("user.email", "t@t")):
            git(self.dev, "config", k, v)
        (self.dev / "crates" / "a").mkdir(parents=True)
        (self.dev / "Cargo.toml").write_text('[workspace.package]\nversion = "0.7.10"\nedition = "2024"\n')
        (self.dev / "crates" / "a" / "Cargo.toml").write_text(
            '[package]\nname = "a"\n[dependencies]\nb = { path = "../b", version = "0.7.10" }\n'
            'other = { version = "0.7.10" }\n')
        (self.dev / "Cargo.lock").write_text(
            '[[package]]\nname = "a"\nversion = "0.7.10"\n\n'
            '[[package]]\nname = "other"\nversion = "0.7.10"\nsource = "registry"\n')
        self.commit("start")
        git(self.dev, "tag", "v0.7.10")
        git(self.dev, "push", "-q", "origin", "main", "--tags")
        subprocess.run(["git", "clone", "-q", str(self.origin), str(self.checkout)], check=True)
        for k, v in (("user.name", "pm"), ("user.email", "pm@t")):
            git(self.checkout, "config", k, v)
        self.now = T0
        self.ledger = Ledger(tmp / "log.jsonl", clock=lambda: self.now)
        cfg = copy.deepcopy(CONFIG)
        cfg["release"] = {"freeze_every_days": 7, "uses_required": uses_required, "soak_minutes": soak}
        self("human", "config_applied", config=cfg)
        self.checks, self.said = Checks(), []
        self.pm = Pm(self.ledger, self.checks, self.checkout, clock=lambda: self.now,
                     publish=list(publish) if publish else None, say=self.said.append)

    def __call__(self, actor, event, **fields):
        self.now += timedelta(minutes=1)
        return self.ledger.submit(actor, json.dumps({"event": event, **fields}))

    def commit(self, name):
        (self.dev / f"{name}.txt").write_text(name)
        git(self.dev, "add", "-A")
        git(self.dev, "commit", "-q", "-m", name)
        return git(self.dev, "rev-parse", "HEAD")

    def fix(self, issue, pr):
        """A bug found, fixed and merged to main."""
        self("human", "filed", issue=issue, kind="bug")
        if self.ledger.world.state(M.AGENT, C1) in (None, "off"):
            self("watchdog", "session_started", agent=C1)
        sha = self.commit(f"fix-{issue}")
        git(self.dev, "push", "-q", "origin", "main")
        self(C1, "taken", issue=issue)
        self(C1, "push", issue=issue, pr=pr, push=1, sha=sha)
        self("ci", "ci_done", pr=pr, push=1, result="pass")
        self("reviewer", "review_done", by="placeholder", pr=pr, push=1, result="approved")
        self("merger", "merged", pr=pr, push=1, sha=sha)
        return sha

    def events(self):
        return [json.loads(l)["event"] for l in open(self.ledger.path)][1:]

    def branch(self, path):
        return git(self.origin, "show", f"release/0.7.11:{path}")


def test_it_freezes_builds_and_publishes(tmp_path):
    f = Factory(tmp_path)
    main = git(f.dev, "rev-parse", "HEAD")
    f.pm.step()
    assert f.events() == ["frozen", "built", "published"]
    w = f.ledger.world
    assert w.is_(M.VERSION, "0.7.11", "published")
    assert git(f.origin, "rev-parse", "release/0.7.11~1") == main
    assert 'version = "0.7.11"' in f.branch("Cargo.toml")
    assert 'b = { path = "../b", version = "0.7.11" }' in f.branch("crates/a/Cargo.toml")
    assert 'other = { version = "0.7.10" }' in f.branch("crates/a/Cargo.toml")
    lock = f.branch("Cargo.lock")
    assert 'name = "a"\nversion = "0.7.11"' in lock and 'name = "other"\nversion = "0.7.10"' in lock
    f.pm.step()  # nothing more for a week
    assert f.events() == ["frozen", "built", "published"]
    f.now += timedelta(days=7)
    f.pm.step()
    assert f.events()[3] == "frozen" and "0.7.12" in f.ledger.world.versions


def test_no_build_until_ci_passes(tmp_path):
    f = Factory(tmp_path)
    f.checks.result = None
    f.pm.step()
    assert f.events() == ["frozen"]
    f.checks.result = "failure"
    f.pm.step()
    f.pm.step()  # too soon to run it again
    assert f.events() == ["frozen"] and f.checks.reruns == 1
    for _ in range(5):
        f.now += timedelta(minutes=21)
        f.pm.step()
    assert f.checks.reruns == 3 and any("4 times" in s for s in f.said)
    f.checks.result = "success"
    f.pm.step()
    assert f.events() == ["frozen", "built", "published"]


def test_it_holds_for_the_reasons_the_world_gives_and_says_so_once(tmp_path):
    f = Factory(tmp_path, uses_required=1, soak=120)
    f.pm.step()
    f.pm.step()
    assert f.events() == ["frozen", "built", "release_held"]
    held = [json.loads(l) for l in open(f.ledger.path)][-1]
    assert sorted(held["reasons"]) == ["soak", "uses"]
    f.now += timedelta(minutes=121)
    f.pm.step()  # the soak is over: the reasons changed
    assert f.events()[-1] == "release_held"
    f("power_user", "use_started", use=1, version="0.7.11", build=1)
    f("power_user", "use_ended", use=1)
    f.pm.step()
    assert f.events()[-1] == "published"


def test_a_bug_fixed_after_the_freeze_is_backported_and_built_again(tmp_path):
    f = Factory(tmp_path, uses_required=1)
    f.pm.step()
    sha = f.fix(issue=5, pr=50)
    f.pm.step()
    assert f.events()[-2:] == ["backported", "release_held"]  # held: a backport in no build
    f.pm.step()
    ev = f.events()
    assert ev.count("backported") == 1 and ev.count("built") == 2
    assert f"cherry picked from commit {sha}" in git(f.origin, "log", "-1", "--format=%B", "release/0.7.11")
    assert f.ledger.world.versions["0.7.11"].unbuilt == set()


def test_a_fix_that_does_not_apply_is_left_out_and_said(tmp_path):
    f = Factory(tmp_path, uses_required=1)
    f.pm.step()
    (f.dev / "Cargo.toml").write_text('[workspace.package]\nversion = "0.7.10"\nedition = "2027"\n')
    git(f.checkout, "checkout", "-q", "release/0.7.11")
    (f.checkout / "Cargo.toml").write_text('[workspace.package]\nversion = "0.7.11"\nedition = "2030"\n')
    git(f.checkout, "commit", "-q", "-am", "diverge")
    git(f.checkout, "push", "-q", "origin", "release/0.7.11")
    f.fix(issue=5, pr=50)
    f.pm.step()
    assert "backported" not in f.events()
    assert any("does not apply" in s for s in f.said)
    assert git(f.checkout, "status", "--porcelain") == ""


def test_a_freeze_whose_branch_was_never_pushed_is_finished(tmp_path):
    f = Factory(tmp_path)
    f("pm", "frozen", version="0.7.11", sha=git(f.dev, "rev-parse", "HEAD"))
    f.pm.step()
    assert f.events() == ["frozen", "built", "published"]


def test_with_no_release_command_it_publishes_nothing(tmp_path):
    f = Factory(tmp_path, publish=None)
    f.pm.step()
    assert f.events() == ["frozen", "built"]
    assert any("nothing published" in s for s in f.said)


def test_a_failed_release_is_not_recorded(tmp_path):
    f = Factory(tmp_path, publish=("false",))
    f.pm.step()
    assert f.events() == ["frozen", "built"]


def test_a_released_version_gets_its_notes_written_once(tmp_path):
    f = Factory(tmp_path)
    asked = []
    def write(version, previous, out):
        asked.append((version, previous))
        out.write_text("## The build server no longer hangs\n" + "It did before. " * 30)
        return 0
    f.pm = Pm(f.ledger, f.checks, f.checkout, clock=lambda: f.now, publish=["true"], say=f.said.append,
              notes_prompt="p", state=tmp_path / "state", write_notes=write)
    f.pm.step()
    assert not asked  # published in the log, but no tag yet
    git(f.dev, "fetch", "-q", "origin")
    git(f.dev, "tag", "v0.7.11", "origin/release/0.7.11")
    git(f.dev, "push", "-q", "origin", "v0.7.11")
    f.pm.step()
    assert not asked  # a tag, but GitHub has no release for it yet
    f.checks.released = ("v0.7.11",)
    f.pm.step()
    f.pm.step()
    assert asked == [("0.7.11", "v0.7.10")]
    assert f.checks.notes[0] == "v0.7.11" and "no longer hangs" in f.checks.notes[1]
    assert (tmp_path / "state" / "notes" / "0.7.11.md").exists()
    f.commit("by hand")  # a release the owner cut from main, which the log does not know
    git(f.dev, "tag", "v0.7.12")
    git(f.dev, "push", "-q", "origin", "main", "v0.7.12")
    f.checks.released = ("v0.7.10", "v0.7.11", "v0.7.12")
    f.pm.step()
    assert asked[1:] == [("0.7.12", "v0.7.11")]  # and not 0.7.10: only the two newest


def test_notes_a_session_did_not_write_are_not_put_on_the_release(tmp_path):
    f = Factory(tmp_path)
    tries = []
    f.pm = Pm(f.ledger, f.checks, f.checkout, clock=lambda: f.now, publish=["true"], say=f.said.append,
              notes_prompt="p", state=tmp_path / "state",
              write_notes=lambda v, p, out: tries.append(v) or 0)
    f.pm.step()
    git(f.dev, "fetch", "-q", "origin")
    git(f.dev, "tag", "v0.7.11", "origin/release/0.7.11")
    git(f.dev, "push", "-q", "origin", "v0.7.11")
    f.checks.released = ("v0.7.11",)
    for _ in range(5):
        f.pm.step()
    assert len(tries) == 3 and f.checks.notes is None


def test_bump_leaves_other_packages(tmp_path):
    (tmp_path / "Cargo.toml").write_text('version = "1.0.0"\ndep = { version = "1.0.0" }\n')
    bump(tmp_path, "1.0.0", "1.0.1")
    assert (tmp_path / "Cargo.toml").read_text() == 'version = "1.0.1"\ndep = { version = "1.0.0" }\n'


def test_a_tag_moved_on_origin_does_not_stop_it(tmp_path):
    f = Factory(tmp_path, uses_required=1)
    f.pm.step()
    f.commit("more")
    git(f.dev, "tag", "-f", "v0.7.10")
    git(f.dev, "push", "-q", "-f", "origin", "main", "--tags")
    f.pm.step()  # git refuses to move a tag it has, unless forced
