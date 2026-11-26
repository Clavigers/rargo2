"""The factory's entities, each a declared state machine.

The world holds one current state per (machine, key). A state changes only
along an edge declared here, and every change closes one interval and opens
the next, so the time each thing spent in each state is a by-product of
checking the log (rule 9), never a separate record.
"""
from __future__ import annotations

import enum
from dataclasses import dataclass


@dataclass(frozen=True)
class Machine:
    name: str
    key: str  # what names one instance; "" for a machine with one instance
    states: type[enum.StrEnum]
    initial: frozenset[enum.StrEnum]
    edges: frozenset[tuple[enum.StrEnum, enum.StrEnum]]
    meaning: str

    def allows(self, a: enum.StrEnum | None, b: enum.StrEnum) -> bool:
        return b in self.initial if a is None else (a, b) in self.edges


def machine(name, key, states, initial, edges, meaning) -> Machine:
    S = states
    return Machine(name, key, S, frozenset(S(s) for s in initial),
                   frozenset((S(a), S(b)) for a, b in edges), meaning)


class IssueS(enum.StrEnum):
    parked = "parked"  # waiting, on hold by the owner: nobody takes it
    blocked = "blocked"  # waiting, with an open dependency
    ready = "ready"  # waiting, nothing in the way
    held = "held"  # in an agent's slot
    checking = "checking"  # pushed; CI and review on the latest push
    closed = "closed"


class InitiativeS(enum.StrEnum):
    open = "open"
    closed = "closed"


class AgentS(enum.StrEnum):
    off = "off"
    idle = "idle"
    working = "working"


class QuotaS(enum.StrEnum):
    ok = "ok"
    out = "out"


class CiS(enum.StrEnum):
    pending = "pending"
    passed = "pass"
    failed = "fail"
    void = "void"  # a newer push came before a result


class ReviewS(enum.StrEnum):
    pending = "pending"
    approved = "approved"
    changes = "changes"
    void = "void"  # CI failed or a newer push came before a result


class PrS(enum.StrEnum):
    open = "open"
    merged = "merged"
    abandoned = "abandoned"  # its issue closed without it


class DictatorS(enum.StrEnum):
    resting = "resting"
    ticking = "ticking"


class VersionS(enum.StrEnum):
    candidate = "candidate"
    published = "published"


class UseS(enum.StrEnum):
    open = "open"
    ended = "ended"


ISSUE = machine(
    "issue", "issue", IssueS, ["blocked", "ready"],
    [("blocked", "ready"), ("ready", "blocked"),
     ("ready", "parked"), ("blocked", "parked"), ("parked", "ready"), ("parked", "blocked"),
     ("parked", "closed"), ("held", "parked"), ("checking", "parked"),
     ("ready", "held"), ("held", "ready"), ("held", "blocked"),
     ("held", "checking"), ("held", "closed"),
     ("checking", "ready"), ("checking", "blocked"),
     ("checking", "closed"),
     ("ready", "closed"), ("blocked", "closed")],
    "A unit of work: enhancement, bug or friction. Waiting with a pull "
    "request means needs work.")
INITIATIVE = machine(
    "initiative", "issue", InitiativeS, ["open"], [("open", "closed")],
    "A larger goal, never worked on directly.")
AGENT = machine(
    "agent", "agent", AgentS, ["off"],
    [("off", "idle"), ("off", "working"), ("idle", "working"),
     ("working", "idle"), ("idle", "off"), ("working", "off")],
    "One agent slot. Off while holding an issue means the issue is stale.")
QUOTA = machine(
    "quota", "agent", QuotaS, ["ok"], [("ok", "out"), ("out", "ok")],
    "Whether the agent's plan lets it run.")
CI = machine(
    "ci", "pr/push", CiS, ["pending"],
    [("pending", "pass"), ("pending", "fail"), ("pending", "void")],
    "CI on one push.")
REVIEW = machine(
    "review", "pr/push/reviewer", ReviewS, ["pending"],
    [("pending", "approved"), ("pending", "changes"), ("pending", "void")],
    "One reviewer of the config on one push.")
PR = machine(
    "pr", "pr", PrS, ["open"], [("open", "merged"), ("open", "abandoned")],
    "The one pull request of an issue.")
DICTATOR = machine(
    "dictator", "", DictatorS, ["resting"],
    [("resting", "ticking"), ("ticking", "resting")],
    "The dictator acts only while ticking.")
VERSION = machine(
    "version", "version", VersionS, ["candidate"], [("candidate", "published")],
    "A release branch and its candidate builds.")
USE = machine(
    "use", "use", UseS, ["open"], [("open", "ended")],
    "The human or the power user using one build.")

MACHINES = (ISSUE, INITIATIVE, AGENT, QUOTA, CI, REVIEW, PR, DICTATOR, VERSION, USE)
