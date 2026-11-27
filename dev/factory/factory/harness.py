"""How to run one headless session of each kind of coding agent.

A session is one process: it gets the prompt, works one issue, records what it
does with `factory submit`, and exits. Each is one conversation the harness
keeps on disk under an id; a later session can resume it, so an agent woken
for work that came back still knows what it did. The binaries are found on PATH, or
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
    # (binary, model, prompt, new conversation id or None, id to resume or None) -> argv
    argv: Callable[[str, str, str, str | None, str | None], list[str]]
    names_itself: bool = False  # it picks its own conversation id and prints it
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
        lambda b, model, p, new, old: [b, "-p", p, "--model", model, "--dangerously-skip-permissions",
                                       *(["--resume", old] if old else ["--session-id", new] if new else [])],
        env={"IS_SANDBOX": "1"}),  # lets it skip permissions as root
    Harness.codex: Kind(
        Harness.codex, "codex",
        lambda b, model, p, new, old: (
            [b, "exec", "resume", old, p, "-m", model, "--dangerously-bypass-approvals-and-sandbox"]
            if old else [b, "exec", "-m", model, "--yolo", p]),
        names_itself=True),
    Harness.grok: Kind(
        Harness.grok, "grok",
        lambda b, model, p, new, old: [b, "-p", p, "-m", model, "--always-approve",
                                       *(["--resume", old] if old else ["-s", new] if new else [])]),
}


SESSION_ID = re.compile(r"session id:\s*([0-9a-f-]{36})", re.I)


def conversation(t: m.AgentType, output: str) -> str | None:
    """The conversation id a harness that names its own printed at the start."""
    found = SESSION_ID.search(output[:4000])
    return found.group(1) if KINDS[t.type].names_itself and found else None


def names_itself(t: m.AgentType) -> bool:
    return KINDS[t.type].names_itself


def command(t: m.AgentType, prompt: str, new: str | None = None,
            resume: str | None = None) -> tuple[list[str], dict[str, str]] | None:
    """argv and extra environment for one session of an agent of type t, or
    None if its harness is not installed here. new: the id to give a new
    conversation; resume: the id of the conversation to continue."""
    k = KINDS[t.type]
    binary = k.resolve()
    if binary is None:
        return None
    return k.argv(binary, t.model.value, prompt, new, resume), dict(k.env)


def session_end(t: m.AgentType, returncode: int, output: str) -> m.SessionEnd:
    """Why a session's process exited, from its exit code and the end of its
    output. Only a failed exit is read for quota, so an agent talking about
    limits in a session that finished is not mistaken for one that hit one."""
    if returncode == 0:
        return m.SessionEnd.finished
    if returncode < 0 or returncode >= 128:  # a signal, directly or through a shell
        return m.SessionEnd.killed
    if KINDS[t.type].quota.search(output[-4000:]):
        return m.SessionEnd.quota
    return m.SessionEnd.crashed
