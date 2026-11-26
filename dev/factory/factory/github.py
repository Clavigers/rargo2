"""The little of GitHub the factory uses, through the gh CLI. Tests use a fake
with the same methods."""
from __future__ import annotations

import json
import subprocess
from datetime import datetime


class GitHubError(Exception):
    pass


class GitHub:
    def __init__(self, repo: str):
        self.repo = repo  # owner/name

    def _gh(self, *args: str, input: str | None = None) -> str:
        r = subprocess.run(["gh", *args], capture_output=True, text=True, input=input)
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
        q = f"state=all&since={since:%Y-%m-%dT%H:%M:%SZ}" if since else "state=open"
        return [i for i in self._api(f"issues?{q}&per_page=100", paginate=True)
                if "pull_request" not in i]

    def pull(self, n: int) -> dict:
        return self._api(f"pulls/{n}")

    def check_runs(self, sha: str) -> list[dict]:
        return self._api(f"commits/{sha}/check-runs?per_page=100")["check_runs"]

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
