"""The project manager: it turns what merged into releases.

Every few minutes it does the next thing that is due, and records it:

1. Freeze. When no version is a candidate and freeze_every_days have passed
   since the last freeze, the next version (one patch above every tag,
   manifest and earlier version) is frozen at main's head. Its branch is
   release/X.Y.Z: that commit plus one that sets the version.
2. Backport. A bug fix merged to main after the freeze is cherry-picked onto
   the branch.
3. Build. The branch's head, once CI passed on it, is the next candidate
   build. CI that failed there is run again, three times at most.
4. Hold or publish. The world says which reasons hold the latest build
   (known bugs, too few power user sessions, too little soak, a backport in
   no build). When the reasons change it records them; when there are none
   it publishes: it runs the release command, and records `published` only
   if that succeeded. With no release command set it publishes nothing and
   says so.

It decides nothing a rule does not: what holds a release is the world's
hold_reasons, and the log refuses a `published` they forbid. Every step can
be repeated: one that stopped half way is finished by the next.
"""
from __future__ import annotations

import json
import re
import subprocess
import time
from datetime import datetime, timedelta
from pathlib import Path
from typing import Callable

from . import machines as M
from . import model as m
from .github import GitHub, GitHubError
from .ledger import Alarm, Ledger, Refused, utc_now
from .model import Kind, Role
from .notify import notify
from .observer import FAILED, PASSED

RERUNS = 3  # times a failed CI on a release branch is run again
RERUN_AFTER = timedelta(minutes=20)  # between them, so a broken service can recover
SEMVER = re.compile(r"^v?(\d+)\.(\d+)\.(\d+)$")


def branch(version: str) -> str:
    return f"release/{version}"


def bump(root: Path, old: str, new: str) -> None:
    """Set the workspace's version: the manifests' own version and their
    versions of each other, and the workspace's packages in Cargo.lock."""
    was, now = f'version = "{old}"', f'version = "{new}"'
    for f in [root / "Cargo.toml", *sorted(root.glob("crates/*/Cargo.toml"))]:
        lines = f.read_text().split("\n")
        f.write_text("\n".join(l.replace(was, now) if l.strip() == was or "path = " in l else l
                               for l in lines))
    lock = root / "Cargo.lock"
    if lock.exists():  # a package with no source is the workspace's own
        blocks = lock.read_text().split("[[package]]")
        lock.write_text("[[package]]".join(
            b if "source = " in b else b.replace(was, now) for b in blocks))


class Pm:
    def __init__(self, ledger: Ledger, gh: GitHub, checkout: Path,
                 clock: Callable[[], datetime] = utc_now,
                 publish: list[str] | None = None,
                 ci_apps=frozenset({"github-actions"}),
                 say: Callable[[str], None] = print):
        self.ledger, self.gh, self.checkout = ledger, gh, Path(checkout)
        self.clock, self.publish, self.ci_apps, self.say = clock, publish, ci_apps, say
        self.told: set[str] = set()  # things said once
        self.reruns: dict[str, list[datetime]] = {}  # commit -> when its CI was run again

    def run(self, every: float = 300.0) -> None:
        while True:
            try:
                self.step()
            except (GitHubError, subprocess.CalledProcessError, Alarm) as err:
                self.say(f"pm: {err}")  # the next step tries again
            time.sleep(every)

    # ------------------------------------------------------------ the step

    def step(self) -> None:
        w = self.ledger.refresh()
        if w.config is None:
            return
        self._git("fetch", "-q", "--prune", "--tags", "origin")
        version = next((v for v in w.versions if w.is_(M.VERSION, v, "candidate")), None)
        if version is None:
            version = self._freeze()
            if version is None:
                return
        self._branch(version)
        self._backport(version)
        self._build(version)
        self._hold_or_publish(version)

    def _freeze(self) -> str | None:
        w = self.ledger.world
        last = max((v.frozen for v in w.versions.values()), default=None)
        if last and self.clock() - last < timedelta(days=w.config.release.freeze_every_days):
            return None
        sha = self._git("rev-parse", "origin/main")
        version = self._next(sha)
        return version if self._record("frozen", version=version, sha=sha) else None

    def _next(self, sha: str) -> str:
        seen = [self._manifest(sha), *self.ledger.world.versions,
                *self._git("tag", "--list", "v*").split()]
        keys = [tuple(map(int, g.groups())) for s in seen if (g := SEMVER.match(s))]
        a, b, c = max(keys)
        return f"{a}.{b}.{c + 1}"

    def _manifest(self, ref: str) -> str:
        toml = self._git("show", f"{ref}:Cargo.toml")
        return re.search(r'^version = "([^"]+)"', toml, re.M).group(1)

    def _branch(self, version: str) -> None:
        """The version's branch exists on origin: the frozen commit, then
        the commit that names the version."""
        b = branch(version)
        if self._has(f"origin/{b}"):
            self._git("checkout", "-q", "-B", b, f"origin/{b}")
            return
        sha = next(l["sha"] for l in self._lines("frozen") if l["version"] == version)
        self._git("checkout", "-q", "-B", b, sha)
        old = self._manifest(sha)
        if old != version:
            bump(self.checkout, old, version)
            self._git("commit", "-q", "-am", version)
        self._git("push", "-q", "origin", f"{b}:{b}")
        self.say(f"pm: {b} cut from {sha[:7]}")

    def _backport(self, version: str) -> None:
        w = self.ledger.world
        v, b = w.versions[version], branch(version)
        shas = {l["pr"]: l["sha"] for l in self._lines("merged")}
        due = sorted((p.merged, pr) for pr, p in w.prs.items()
                     if p.merged and p.merged >= v.frozen and pr not in v.backports
                     and w.issues[p.issue].kind is Kind.bug and pr in shas)
        for _, pr in due:
            sha = shas[pr]
            if f"in {b} {sha}" in self.told:
                continue
            if self._ok("merge-base", "--is-ancestor", sha, "HEAD"):
                self.told.add(f"in {b} {sha}")  # merged just before the cut: it is in
                continue
            if not self._ok("cherry-pick", "-x", sha):
                self._ok("cherry-pick", "--abort")
                self._once(f"pr {pr} does not apply to {b}; {version} goes without it "
                           f"until someone backports it by hand", tell=True)
                continue
            self._git("push", "-q", "origin", f"{b}:{b}")
            self._record("backported", version=version, pr=pr)

    def _build(self, version: str) -> None:
        w = self.ledger.refresh()
        head = self._git("rev-parse", "HEAD")
        built = [l["sha"] for l in self._lines("built") if l["version"] == version]
        if built and built[-1] == head:
            return
        ci = self._ci(head)
        if ci == "pass":
            self._record("built", version=version, build=len(w.versions[version].builds) + 1, sha=head)
        elif ci == "fail":
            self._failed(version, head)

    def _failed(self, version: str, head: str) -> None:
        """CI failed on the branch's head. A release branch holds only what
        passed on main, so the cause is most often not the code: run it
        again, a few times, before saying that it needs someone."""
        tries = self.reruns.setdefault(head, [])
        now = self.clock()
        if len(tries) >= RERUNS:
            self._once(f"CI failed on {branch(version)} at {head[:7]} {RERUNS + 1} times; "
                       f"no build until it passes", tell=True)
        elif not tries or now - tries[-1] >= RERUN_AFTER:
            tries.append(now)
            n = self.gh.rerun_failed(head)
            self.say(f"pm: CI failed on {branch(version)} at {head[:7]}; ran {n} run(s) again "
                     f"(try {len(tries)} of {RERUNS})")

    def _ci(self, sha: str) -> str | None:
        runs = [r for r in self.gh.check_runs(sha) if r["app"]["slug"] in self.ci_apps]
        if not runs or any(r["status"] != "completed" for r in runs):
            return None
        if any(r["conclusion"] in FAILED for r in runs):
            return "fail"
        return "pass" if all(r["conclusion"] in PASSED for r in runs) else None

    def _hold_or_publish(self, version: str) -> None:
        w = self.ledger.refresh()
        build = len(w.versions[version].builds)
        if build == 0:
            return
        reasons = sorted(w.hold_reasons(version))
        if reasons:
            held = [l for l in self._lines("release_held")
                    if (l["version"], l["build"]) == (version, build)]
            if not held or sorted(held[-1]["reasons"]) != reasons:
                self._record("release_held", version=version, build=build, reasons=reasons)
            return
        if self.publish is None:
            self._once(f"{version} build {build} is ready to publish, and no release "
                       f"command is set: nothing published", tell=True)
            return
        try:  # ask first: the release cannot be taken back
            self.ledger.submit(Role.pm, json.dumps(
                {"event": "published", "version": version, "build": build}), dry=True)
        except Refused as err:
            self.say(f"pm: {err}")
            return
        r = subprocess.run([*self.publish, version], cwd=self.checkout)
        if r.returncode != 0:
            self._once(f"the release command failed for {version} build {build} "
                       f"(exit {r.returncode})", tell=True)
            return
        self._record("published", version=version, build=build)

    # ------------------------------------------------------------ helpers

    def _record(self, event: str, **fields) -> bool:
        try:
            self.ledger.submit(Role.pm, json.dumps({"event": event, **fields}))
            self.say(f"pm: {event} {fields}")
            return True
        except Refused as err:
            self.say(f"pm: {err}")
            return False

    def _lines(self, event: str) -> list[dict]:
        key = f'"event":"{event}"'
        with open(self.ledger.path) as f:
            return [json.loads(l) for l in f if key in l]

    def _once(self, text: str, tell: bool = False) -> None:
        if text not in self.told:
            self.told.add(text)
            self.say(f"pm: {text}")
            if tell:
                notify(text, "rargo2 factory: release")

    def _git(self, *args: str) -> str:
        return subprocess.run(["git", "-C", str(self.checkout), *args], check=True,
                              capture_output=True, text=True).stdout.strip()

    def _ok(self, *args: str) -> bool:
        return subprocess.run(["git", "-C", str(self.checkout), *args],
                              capture_output=True).returncode == 0

    def _has(self, ref: str) -> bool:
        return self._ok("rev-parse", "--verify", "-q", ref)
