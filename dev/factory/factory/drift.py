"""Whether the factory's running code is the code on main.

The services run the copy of dev/factory at /root/factory/src, made by hand
on the host; nothing deploys a change merged under dev/factory. `factory
status` says when the copy differs from dev/factory on the main the host's
clone can see, so the dictator sees it at once. Deploying stays a host
action."""
from __future__ import annotations

import hashlib
import subprocess
from pathlib import Path

DIR = "dev/factory"
SKIP = {"__pycache__", ".pytest_cache", ".venv", ".mypy_cache", ".ruff_cache"}


def blob(data: bytes) -> str:
    """git's object id of a file with this content."""
    return hashlib.sha1(b"blob %d\0" % len(data) + data).hexdigest()


def _git(repo: Path, *args: str) -> str:
    return subprocess.run(["git", "-C", str(repo), *args], capture_output=True, text=True,
                          check=True).stdout


def drift(here: Path, repo: Path, ref: str = "origin/main") -> list[str]:
    """The files, relative to dev/factory, that differ between the copy here
    and dev/factory at ref in the clone repo: changed, missing here, or here
    and not on main."""
    on_main = {}
    for row in _git(repo, "ls-tree", "-r", "-z", ref, "--", DIR).split("\0"):
        if row:
            meta, path = row.split("\t", 1)
            on_main[path[len(DIR) + 1:]] = meta.split()[2]
    here_files = {}
    for f in here.rglob("*"):
        rel = f.relative_to(here)
        if f.is_file() and not SKIP & set(rel.parts) and f.suffix != ".pyc" \
                and not rel.parts[0].endswith(".egg-info"):
            here_files[rel.as_posix()] = f
    return sorted(p for p in on_main.keys() | here_files.keys()
                  if p not in on_main or p not in here_files
                  or blob(here_files[p].read_bytes()) != on_main[p])


def line(here: Path, repo: Path, ref: str = "origin/main") -> str | None:
    """One line for `factory status` when the running code is not main's, or
    when it cannot tell; None when they are the same."""
    try:
        files = drift(here, repo, ref)
        sha = _git(repo, "rev-parse", "--short", ref).strip()
    except (OSError, subprocess.CalledProcessError) as err:
        detail = getattr(err, "stderr", "") or str(err)
        return f"deploy: cannot compare {here} with {DIR} on {ref} in {repo}: {detail.strip()}"
    if not files:
        return None
    return (f"deploy: {here} differs from {DIR} on {ref} ({sha}), {len(files)} "
            f"file{'s' * (len(files) != 1)}: {', '.join(files)}")
