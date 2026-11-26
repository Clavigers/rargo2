"""How to run one headless session of each kind of coding agent.

A session is one process: it gets the prompt, works one issue, records what it
does with `factory submit`, and exits. The binaries are found on PATH, or
where FACTORY_<HARNESS> (FACTORY_CLAUDE_CODE, FACTORY_CODEX, FACTORY_GROK)
points.
"""
from __future__ import annotations

import os
import re
import shutil
from dataclasses import dataclass, field
from typing import Callable

from . import model as m
from .model import Harness


@dataclass(frozen=True)
class Kind:
    harness: Harness
    binary: str
    argv: Callable[[str, str, str], list[str]]  # (binary, model, prompt) -> argv
    env: dict[str, str] = field(default_factory=dict)
    # Output that means the plan's quota ran out, not that the session failed.
    quota: re.Pattern = re.compile(
        r"usage limit|rate limit|out of (credits|quota)|limit (will )?resets?"
        r"|try again (at|in)|balance exhausted|402 Payment Required", re.I)

    def resolve(self) -> str | None:
        override = os.environ.get("FACTORY_" + self.harness.name.upper())
        return override or shutil.which(self.binary)


KINDS: dict[Harness, Kind] = {
    Harness.claude_code: Kind(
        Harness.claude_code, "claude",
        lambda b, model, p: [b, "-p", p, "--model", model, "--dangerously-skip-permissions"],
        env={"IS_SANDBOX": "1"}),  # lets it skip permissions as root
    Harness.codex: Kind(
        Harness.codex, "codex",
        lambda b, model, p: [b, "exec", "-m", model, "--yolo", p]),
    Harness.grok: Kind(
        Harness.grok, "grok",
        lambda b, model, p: [b, "-p", p, "-m", model, "--always-approve"]),
}


def command(t: m.AgentType, prompt: str) -> tuple[list[str], dict[str, str]] | None:
    """argv and extra environment for one session of an agent of type t, or
    None if its harness is not installed here."""
    k = KINDS[t.type]
    binary = k.resolve()
    if binary is None:
        return None
    return k.argv(binary, t.model.value, prompt), dict(k.env)


def session_end(t: m.AgentType, returncode: int, output: str) -> m.SessionEnd:
    """Why a session's process exited, from its exit code and the end of its
    output. Only a failed exit is read for quota, so an agent talking about
    limits in a session that finished is not mistaken for one that hit one."""
    if returncode == 0:
        return m.SessionEnd.finished
    if returncode < 0:
        return m.SessionEnd.killed
    if KINDS[t.type].quota.search(output[-4000:]):
        return m.SessionEnd.quota
    return m.SessionEnd.crashed
