"""The reviewer's own append-only JSON-lines record: which bots it asked, what
they answered, what they cost. The state of every review is folded from it,
so a restart resumes and no review is paid for twice. It sits beside the
factory's log; the log records only each review's verdict."""
from __future__ import annotations

import fcntl
import json
import os
from datetime import datetime, timezone


def iso(t: datetime) -> str:
    return t.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


class Record:
    def __init__(self, path: str):
        self.path = path

    def append(self, t: datetime, **fields) -> dict:
        rec = {"t": iso(t), **fields}
        line = json.dumps(rec, sort_keys=False) + "\n"
        with open(self.path, "a") as f:
            fcntl.flock(f, fcntl.LOCK_EX)
            f.write(line)
            f.flush()
            os.fsync(f.fileno())
        return rec

    def read(self) -> list[dict]:
        if not os.path.exists(self.path):
            return []
        with open(self.path) as f:
            return [json.loads(l) for l in f if l.strip()]

    def for_head(self, repo: str, pr: int, sha: str) -> list[dict]:
        return [r for r in self.read()
                if r.get("repo") == repo and r.get("pr") == pr and r.get("sha") == sha]
