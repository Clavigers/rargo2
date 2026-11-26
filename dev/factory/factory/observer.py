"""The observer polls GitHub and records what happened there, and the merger
merges what the log says is ready. Each step compares GitHub with the world
and records the difference, so a step missed (the observer was down, GitHub
was slow) is only late, never lost.

What it records:
- Issues opened by a trusted login (the owner, by default): `filed`, by the
  actor named in the body's `<!-- factory-actor: NAME -->` marker, else by
  the human. Agents and the foreman open issues this way, so the observer is
  the one recorder of `filed`. Issues from anyone else are not the factory's.
- Issues the log has open that are closed on GitHub: `closed`, by the human
  if the owner closed them, else by an outsider.
- For each open pull request the log knows, on the commit of its latest
  recorded push and no other: CI (`ci_done`, from the CI app's check runs),
  each reviewer's verdict (`review_done`), and a merge done on GitHub by
  someone else (`merged`, by the human or an outsider).
- Merges it does itself, as the merger, once CI passed and every reviewer
  approved: `merged`, or `merge_failed`.

An issue body says where an issue sits with lines of its own:
    Part of #N        its initiative
    Depends on #N     an issue it waits for (any number of these)
"""
from __future__ import annotations

import json
import re
from datetime import datetime, timedelta, timezone
from typing import Callable

from . import machines as M
from . import model as m
from .github import GitHub, GitHubError
from .ledger import Alarm, Ledger, Refused
from .notify import notify
from .machines import CiS, IssueS, ReviewS
from .model import Kind, Role
from .world import green

MARKER = re.compile(r"<!--\s*factory-actor:\s*([a-z][a-z0-9_-]*)\s*-->")
PART_OF = re.compile(r"^\s*Part of #(\d+)\s*$", re.M | re.I)
DEPENDS = re.compile(r"^\s*Depends on #(\d+)\s*$", re.M | re.I)
def parse_time(s: str) -> datetime:
    return datetime.fromisoformat(s.replace("Z", "+00:00"))


NEEDS_TYPE = "<!-- factory: needs-type -->"
# How a pull request says which issue it does: "Closes #N" and the like, or
# a title ending "(#N)".
CLOSES = re.compile(r"(?i)\b(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?)\s+#(\d+)|\(#(\d+)\)\s*$", re.M)
PARK = "guard"  # the label by which the owner puts an issue on hold
LOCK = "wip-"  # labels by which others outside the factory lock an issue
FAILED = {"failure", "timed_out", "cancelled", "action_required", "startup_failure"}
PASSED = {"success", "neutral", "skipped"}

# A reviewer: given the pull request and the commit, its verdict on that
# commit, or None while it has none yet.
Reviewer = Callable[[GitHub, int, str], "tuple[m.ReviewResult, str | None] | None"]


def placeholder(gh: GitHub, pr: int, sha: str):
    """Approves every push at once, until a real reviewer is set up."""
    return m.ReviewResult.approved, None


class ReviewerStuck(Exception):
    """A reviewer that cannot give a verdict without the owner."""


ELLIPSIS = "ellipsis-dev[bot]"
ELLIPSIS_WAIT = timedelta(minutes=30)
_first_seen: dict[tuple[int, str], datetime] = {}  # when a push was first found unreviewed


def ellipsis(gh: GitHub, pr: int, sha: str):
    """Ellipsis reviews every push on its own. Its review of this commit is
    the verdict: an approval approves; anything else it says asks for
    changes. No review after ELLIPSIS_WAIT needs the owner."""
    mine = [r for r in gh.reviews(pr) if r["user"]["login"] == ELLIPSIS and r.get("commit_id") == sha]
    if not mine:
        seen = _first_seen.setdefault((pr, sha), datetime.now(timezone.utc))
        if datetime.now(timezone.utc) - seen > ELLIPSIS_WAIT:
            raise ReviewerStuck(f"Ellipsis has not reviewed {sha[:8]} in {ELLIPSIS_WAIT.seconds // 60} minutes")
        return None
    _first_seen.pop((pr, sha), None)
    last = mine[-1]
    if last["state"] == "APPROVED":
        return m.ReviewResult.approved, None
    return m.ReviewResult.changes, f"Ellipsis: {last.get('html_url', 'see its review on the pull request')}"


REVIEWERS: dict[str, Reviewer] = {"placeholder": placeholder, "ellipsis": ellipsis}


class Observer:
    def __init__(self, ledger: Ledger, gh: GitHub, owner: str,
                 trusted: frozenset[str] = frozenset(), ci_apps=frozenset({"github-actions"}),
                 merge: bool = True, say: Callable[[str], None] = print,
                 reviewers: dict[str, Reviewer] | None = None):
        self.ledger, self.gh, self.owner = ledger, gh, owner
        self.trusted = trusted | {owner}
        self.ci_apps, self.merging, self.say = ci_apps, merge, say
        self.reviewers = {**REVIEWERS, **(reviewers or {})}
        self.since: datetime | None = None
        self.asked: set[int] = set()  # issues we asked for a type label
        self.noted: set[tuple] = set()  # things said once
        self.pr_locks: set[int] = set()  # issues outside pull requests are working on

    def step(self) -> None:
        start = datetime.now(timezone.utc)
        w = self.ledger.refresh()
        for pr in [n for n in w.prs if w.is_(M.PR, n, "open")]:
            try:
                self._pull(pr)
            except GitHubError as err:
                self.say(f"pr {pr}: {err}")
        self._issues()
        try:
            self._outside_prs()
        except GitHubError as err:
            self.say(f"outside pull requests: {err}")
        self.since = start - timedelta(minutes=5)  # overlap: GitHub's since is not exact

    # ------------------------------------------------------------ pull requests

    def _pull(self, pr: int) -> None:
        w = self.ledger.world
        p = w.prs[pr]
        g = self.gh.pull(pr)
        if g.get("merged_at"):
            by = (g.get("merged_by") or {}).get("login")
            self._record(self._who(by), "merged", pr=pr, push=p.pushes, sha=g["merge_commit_sha"])
            return
        if g["state"] == "closed":
            self._record(Role.merger, "diverged", pr=pr, push=p.pushes, reason="closed")
            return
        if g["head"]["sha"] != p.sha:
            if w.is_(M.ISSUE, p.issue, IssueS.checking):  # not an agent's push in flight
                self._record(Role.merger, "diverged", pr=pr, push=p.pushes, reason="foreign_push")
                return
            self._once(("head", pr, g["head"]["sha"]),
                       f"pr {pr}: its head {g['head']['sha'][:8]} is not the recorded push "
                       f"{p.pushes} ({p.sha[:8]}); waiting for a recorded push")
            return
        key = f"{pr}/{p.pushes}"
        if w.is_(M.CI, key, CiS.pending):
            result = self._ci(p.sha)
            if result:
                self._record(Role.ci, "ci_done", pr=pr, push=p.pushes, result=result)
        for r in p.reviewers:
            if self.ledger.world.is_(M.REVIEW, f"{key}/{r}", ReviewS.pending):
                try:
                    verdict = self.reviewers[r](self.gh, pr, p.sha) if r in self.reviewers else None
                except ReviewerStuck as err:
                    verdict = None
                    if ("stuck", pr, p.sha) not in self.noted:
                        notify(f"pr {pr}: {err}", "rargo factory: review stuck")
                    self._once(("stuck", pr, p.sha), str(err))
                except GitHubError as err:
                    verdict = None
                    self.say(f"pr {pr}: reviewer {r}: {err}")
                if r not in self.reviewers:
                    self._once(("reviewer", r), f"no reviewer called {r} is wired up")
                if verdict:
                    result, note = verdict
                    fields = {"reasons": ["other"]} if result is m.ReviewResult.changes else {}
                    self._record(Role.reviewer, "review_done", by=r, pr=pr, push=p.pushes,
                                 result=result.value, **fields, **({"note": note} if note else {}))
        w = self.ledger.world
        if self.merging and green(w, pr) and w.is_(M.ISSUE, p.issue, IssueS.checking):
            self._merge(pr, p.pushes, p.sha)

    def _ci(self, sha: str) -> str | None:
        runs = [r for r in self.gh.check_runs(sha) if r["app"]["slug"] in self.ci_apps]
        if not runs or any(r["status"] != "completed" for r in runs):
            return None
        if any(r["conclusion"] in FAILED for r in runs):
            return "fail"
        if all(r["conclusion"] in PASSED for r in runs):
            return "pass"
        return None

    def _merge(self, pr: int, push: int, sha: str) -> None:
        try:
            self.gh.merge(pr, sha)
        except GitHubError as err:
            g = self.gh.pull(pr)
            if g["head"]["sha"] != sha or g.get("merged_at"):
                return  # it moved under us; the next step sees what happened
            reason = "conflict" if g.get("mergeable_state") == "dirty" else "stale_base"
            self.say(f"pr {pr}: merge failed ({reason}): {err}")
            self._record(Role.merger, "merge_failed", pr=pr, push=push, reason=reason)
            return
        g = self.gh.pull(pr)
        self._record(Role.merger, "merged", pr=pr, push=push, sha=g["merge_commit_sha"])

    # ------------------------------------------------------------ issues

    def _issues(self) -> None:
        try:
            issues = self.gh.issues(self.since)
        except GitHubError as err:
            self.say(f"issues: {err}")
            return
        new: dict[int, dict] = {}
        for i in sorted(issues, key=lambda i: i["number"]):
            n = i["number"]
            w = self.ledger.world
            if n in w.issues:
                if i["state"] == "closed" and w.open_(n):
                    self._closed(i)
                elif i["state"] == "open":
                    self._park(i)
            elif i["state"] == "open" and i["user"]["login"] in self.trusted:
                new[n] = i
        # File an issue after the new ones it names, so its parent and
        # dependencies are in the log when it is checked.
        while new:
            batch = [n for n, i in new.items() if not self._refs(i) & new.keys()]
            for n in batch or list(new):  # a cycle: file the rest as they are
                i = new.pop(n)
                self._file(i)
                if n in self.ledger.world.issues:
                    self._park(i)

    def _refs(self, i: dict) -> set[int]:
        body = i.get("body") or ""
        return {int(x) for x in PART_OF.findall(body) + DEPENDS.findall(body)}

    def _file(self, i: dict) -> None:
        n, body = i["number"], i.get("body") or ""
        labels = {l["name"] for l in i["labels"]} & set(Kind)
        if len(labels) != 1:
            if n not in self.asked:  # ask once, ever: the comment itself is the record
                try:
                    if not any(NEEDS_TYPE in c for c in self.gh.comments(n)):
                        self.say(f"#{n}: needs exactly one type label")
                        self.gh.comment(n, f"{NEEDS_TYPE}\nThe factory takes this issue up once "
                                           f"it has exactly one of the labels "
                                           f"{', '.join(sorted(Kind))}.")
                    self.asked.add(n)
                except GitHubError as err:
                    self.say(f"#{n}: {err}")
            return
        w = self.ledger.world
        kind = Kind(labels.pop())
        marker = MARKER.search(body)
        actor = marker.group(1) if marker else Role.human
        parents = [int(x) for x in PART_OF.findall(body)]
        parent = next((p for p in parents if p in w.issues
                       and w.issues[p].kind is Kind.initiative and w.open_(p)), None)
        deps = sorted(d for d in map(int, DEPENDS.findall(body))
                      if d in w.issues and w.issues[d].kind is not Kind.initiative and w.open_(d))
        fields = {"issue": n, "kind": kind.value}
        if kind is not Kind.initiative:
            fields |= ({"parent": parent} if parent else {}) | ({"deps": deps} if deps else {})
        self._record(actor, "filed", **fields)

    def _park(self, i: dict) -> None:
        """The owner's hold (the label guard, or the issue assigned to the
    owner) and outside locks (wip- labels, or an open pull request from
    outside the factory that says it closes the issue) follow GitHub."""
        n = i["number"]
        labels = {l["name"] for l in i["labels"]}
        mine = self.owner in {u["login"] for u in i.get("assignees") or []}
        owner = PARK in labels or mine
        locked = any(x.startswith(LOCK) for x in labels) or n in self.pr_locks
        w = self.ledger.world
        parked = w.issues[n].parked
        if w.issues[n].asked:  # its agent asked the owner: their answer lifts it
            asked_at = w.since(M.ISSUE, n)
            if any(c["user"]["login"] == self.owner and parse_time(c["created_at"]) > asked_at
                   for c in self.gh.issue_comments(n)):
                self._record(Role.human, "unparked", issue=n)
            return
        if (owner or locked) and not parked:
            self._record(Role.human if owner else Role.outside, "parked", issue=n,
                         reason="owner" if owner else "lock")
        elif parked and not (owner or locked) and w.issues[n].hold != m.ParkReason.foreman:
            self._record(Role.human, "unparked", issue=n)

    def _outside_prs(self) -> None:
        """An open pull request from anyone but the factory's own accounts that
        names an issue it closes is someone else working on it: an outside
        lock, as long as the pull request is open."""
        w = self.ledger.world
        ours = self.trusted - {self.owner}  # the factory's own GitHub accounts
        locks = set()
        for g in self.gh.open_pulls():
            if g["number"] in w.prs or g["user"]["login"] in ours:
                continue
            for x, y in CLOSES.findall(f"{g.get('title') or ''}\n{g.get('body') or ''}"):
                n = int(x or y)
                if n in w.issues and w.open_(n) and w.issues[n].kind is not Kind.initiative:
                    locks.add(n)
        changed, self.pr_locks = locks ^ self.pr_locks, locks
        for n in sorted(changed):
            if w.open_(n):
                self._park(self.gh.get_issue(n))

    def _closed(self, i: dict) -> None:
        n = i["number"]
        w = self.ledger.world
        actor = self._who((i.get("closed_by") or {}).get("login"))
        initiative = w.issues[n].kind is Kind.initiative
        reason = {"completed": "done" if initiative else "completed",
                  "not_planned": "wontfix"}.get(i.get("state_reason") or "", "wontfix")
        self._record(actor, "closed", issue=n, reason=reason,
                     note=f"closed on GitHub ({i.get('state_reason') or 'no reason'})")

    # ------------------------------------------------------------ recording

    def _who(self, login: str | None) -> str:
        return Role.human if login == self.owner else Role.outside

    def _record(self, actor: str, event: str, **fields) -> bool:
        try:
            self.ledger.submit(actor, json.dumps({"event": event, **fields}))
            return True
        except (Refused, Alarm, ValueError) as err:
            key = ("refused", event, json.dumps(fields, sort_keys=True))
            if isinstance(err, Alarm) and key not in self.noted:
                notify(f"{actor} {event} {fields}: {err}", "rargo factory alarm")
            self._once(key, f"{actor} {event} {fields}: {err}")
            return False

    def _once(self, key: tuple, msg: str) -> None:
        if key not in self.noted:
            self.noted.add(key)
            self.say(msg)
