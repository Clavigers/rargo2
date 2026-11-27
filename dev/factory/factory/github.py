"""The little of GitHub the factory uses, through the gh CLI. Tests use a fake
with the same methods."""
from __future__ import annotations

import json
import os
import subprocess
from datetime import datetime, timezone


class GitHubError(Exception):
    pass


def token_var(account: str) -> str:
    return "GH_TOKEN_" + account.upper().replace("-", "_")


def token_for(account: str) -> str | None:
    """A token to act as account: GH_TOKEN_<ACCOUNT>, else gh's stored one."""
    if os.environ.get(token_var(account)):
        return os.environ[token_var(account)]
    r = subprocess.run(["gh", "auth", "token", "--user", account], capture_output=True, text=True)
    return r.stdout.strip() if r.returncode == 0 and r.stdout.strip() else None


class GitHub:
    def __init__(self, repo: str):
        self.repo = repo  # owner/name

    def _gh(self, *args: str, input: str | None = None, as_: str | None = None) -> str:
        env = None
        if as_:
            tok = token_for(as_)
            if not tok:
                raise GitHubError(f"no token for {as_}: set {token_var(as_)} or gh auth login as {as_}")
            env = dict(os.environ, GH_TOKEN=tok)
        r = subprocess.run(["gh", *args], capture_output=True, text=True, input=input, env=env)
        if r.returncode != 0:
            raise GitHubError(f"gh {' '.join(args[:3])}: {r.stderr.strip()[:500]}")
        return r.stdout

    def _api(self, path: str, paginate: bool = False):
        out = self._gh("api", *(["--paginate", "--slurp"] if paginate else []),
                       f"repos/{self.repo}/{path}")
        data = json.loads(out)
        if paginate:
            return [x for page in data for x in page]
        return data

    def issues(self, since: datetime | None) -> list[dict]:
        """Issues (not pull requests): every open one, or with since, every one
        updated since then, open or closed."""
        if since and since.tzinfo is None:
            raise ValueError("since must be an aware time")
        q = (f"state=all&since={since.astimezone(timezone.utc):%Y-%m-%dT%H:%M:%SZ}"
             if since else "state=open")
        return [i for i in self._api(f"issues?{q}&per_page=100", paginate=True)
                if "pull_request" not in i]

    def open_pulls(self) -> list[dict]:
        return self._api("pulls?state=open&per_page=100", paginate=True)

    def get_issue(self, n: int) -> dict:
        return self._api(f"issues/{n}")

    def pull(self, n: int) -> dict:
        return self._api(f"pulls/{n}")

    def check_runs(self, sha: str) -> list[dict]:
        return self._api(f"commits/{sha}/check-runs?per_page=100")["check_runs"]

    def rerun_failed(self, sha: str) -> int:
        """Run again the failed jobs of the workflow runs on this commit."""
        runs = self._api(f"actions/runs?head_sha={sha}&per_page=100")["workflow_runs"]
        failed = [r["id"] for r in runs if r["status"] == "completed" and r["conclusion"] in
                  ("failure", "timed_out", "cancelled", "startup_failure")]
        for run in failed:
            self._gh("run", "rerun", str(run), "-R", self.repo, "--failed")
        return len(failed)

    def merge(self, pr: int, sha: str) -> None:
        self._gh("pr", "merge", str(pr), "-R", self.repo, "--squash", "--delete-branch",
                 "--match-head-commit", sha)

    def comments(self, issue: int) -> list[str]:
        return [c["body"] for c in self._api(f"issues/{issue}/comments?per_page=100",
                                             paginate=True)]

    def comment(self, issue: int, body: str) -> None:
        self._gh("issue", "comment", str(issue), "-R", self.repo, "--body-file", "-", input=body)

    def close(self, issue: int, reason: str, body: str) -> None:
        """reason: completed or not planned."""
        self._gh("issue", "close", str(issue), "-R", self.repo, "--reason", reason,
                 "--comment", body)

    # what the review-mux reviewer uses

    def issue_comments(self, pr: int) -> list[dict]:
        return self._api(f"issues/{pr}/comments?per_page=100", paginate=True)

    def review_comments(self, pr: int) -> list[dict]:
        return self._api(f"pulls/{pr}/comments?per_page=100", paginate=True)

    def reviews(self, pr: int) -> list[dict]:
        return self._api(f"pulls/{pr}/reviews?per_page=100", paginate=True)

    def change(self, base: str, sha: str) -> list[str]:
        """What sha changes since it left base: the files and the lines added
        and removed, without positions or context, so that merging base in or
        rebasing onto it leaves it the same."""
        diff = self._gh("api", "-H", "Accept: application/vnd.github.diff",
                        f"repos/{self.repo}/compare/{base}...{sha}")
        return [line for line in diff.splitlines() if line[:1] in ("+", "-")]

    def post(self, pr: int, body: str, as_: str | None = None) -> int:
        """A comment on pr, as the account as_; returns its id."""
        out = self._gh("api", f"repos/{self.repo}/issues/{pr}/comments", "-X", "POST",
                       "-F", "body=@-", input=body, as_=as_)
        return json.loads(out)["id"]

    def edit_comment(self, comment_id: int, body: str, as_: str | None = None) -> None:
        self._gh("api", f"repos/{self.repo}/issues/comments/{comment_id}", "-X", "PATCH",
                 "-F", "body=@-", input=body, as_=as_)

    def set_body(self, pr: int, body: str, as_: str | None = None) -> None:
        self._gh("api", f"repos/{self.repo}/pulls/{pr}", "-X", "PATCH",
                 "-F", "body=@-", input=body, as_=as_)
