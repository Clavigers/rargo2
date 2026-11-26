"""What one line of the factory log may look like. Shape only; whether a line
may come next is world.py's question.

A line is an envelope the ledger writes (schema, seq, t, actor) and a body the
actor submits (event and its fields), flat in one JSON object.
"""
from __future__ import annotations

import enum
import json
import re
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import Annotated, ClassVar, Literal, Union

from pydantic import (
    AfterValidator,
    BaseModel,
    BeforeValidator,
    ConfigDict,
    Field,
    PlainSerializer,
    StringConstraints,
    TypeAdapter,
    model_validator,
)

SCHEMA = 1


class Strict(BaseModel):
    model_config = ConfigDict(extra="forbid", strict=True, frozen=True)


def Set(item):
    """A set of item, written as a sorted list with no repeats."""
    def no_repeats(v):
        if not isinstance(v, (list, frozenset)):
            raise ValueError("a set is written as a list")
        if len(set(v)) != len(v):
            raise ValueError("repeats an element")
        return frozenset(v)
    return Annotated[frozenset[item], BeforeValidator(no_repeats),
                     PlainSerializer(sorted, return_type=list)]


# ---------------------------------------------------------------- names

Number = Annotated[int, Field(ge=1)]  # a GitHub issue or pull request number
PushNo = Annotated[int, Field(ge=1)]
Seq = Annotated[int, Field(ge=1)]
Sha = Annotated[str, StringConstraints(pattern=r"^[0-9a-f]{40}$")]
Note = Annotated[str, StringConstraints(min_length=1, max_length=2000)]
Slug = Annotated[str, StringConstraints(pattern=r"^[a-z][a-z0-9]*(-[a-z0-9]+)*$",
                                        max_length=40)]


def _semver(v: str) -> str:
    if not re.fullmatch(r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)", v):
        raise ValueError(f"{v!r} is not MAJOR.MINOR.PATCH")
    return v


Version = Annotated[str, AfterValidator(_semver)]


def version_key(v: str) -> tuple[int, ...]:
    return tuple(int(x) for x in v.split("."))


class Role(enum.StrEnum):
    human = "human"
    foreman = "foreman"
    watchdog = "watchdog"
    ci = "ci"
    reviewer = "reviewer"
    merger = "merger"
    pm = "pm"
    power_user = "power_user"
    # Anyone outside the factory acting on GitHub: other people, other agents.
    # The human is outside the factory too, but by name.
    outside = "outside"


AGENT = "agent"  # in an event's actors: any agent in the config in force
ROLES = frozenset(Role)
Actor = Annotated[str, StringConstraints(pattern=r"^[a-z][a-z0-9]*([-_][a-z0-9]+)*$",
                                         max_length=40)]


def _no_role(n: str) -> str:
    if n in ROLES:
        raise ValueError(f"{n!r} is a role, not an agent")
    return n


AgentName = Annotated[Slug, AfterValidator(_no_role)]

_T = re.compile(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ")


def _t_text(v):
    if isinstance(v, datetime):
        return v
    if not isinstance(v, str) or not _T.fullmatch(v):
        raise ValueError("t is UTC to the second: YYYY-MM-DDTHH:MM:SSZ")
    return datetime.strptime(v, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc)


Time = Annotated[datetime, BeforeValidator(_t_text),
                 PlainSerializer(lambda t: t.strftime("%Y-%m-%dT%H:%M:%SZ"),
                                 return_type=str)]


# ---------------------------------------------------------------- config


class Harness(enum.StrEnum):
    """The kind of worker: the coding agent program a session runs."""

    claude_code = "claude-code"
    codex = "codex"
    grok = "grok"


class Model(enum.StrEnum):
    """Model ids exactly as the harnesses take them."""

    claude_opus_5_5 = "claude-opus-5-5"
    claude_fable_5_2 = "claude-fable-5-2"
    gpt_6_2_sol = "gpt-6.2-sol"
    grok_4_7 = "grok-4.7"


class Plan(Strict):
    name: Slug
    hours_per_week: Annotated[float, Field(gt=0, le=168)]


class AgentType(Strict):
    """count agents of one type, named type-1 .. type-count."""

    type: Harness
    count: Annotated[int, Field(ge=0, le=99)]
    model: Model
    plan: Slug  # each agent has its own quota on this plan

    def names(self) -> list[str]:
        return [f"{self.type}-{i}" for i in range(1, self.count + 1)]


class ForemanRules(Strict):
    tick_minutes: Annotated[int, Field(ge=1)]
    # A bug ready this long pulls an agent off other work. None: never.
    preempt_minutes: Annotated[int, Field(ge=1)] | None
    # Work that comes back (red checks, a conflict, the owner's answer) waits
    # this long for the agent that last held it, which resumes its own
    # conversation, before any agent may take it. None: no waiting.
    affinity_minutes: Annotated[int, Field(ge=1)] | None = None
    # Sessions in a row that end with the holder still holding an issue it
    # did not move; then the foreman takes it back. None: never.
    no_progress_sessions: Annotated[int, Field(ge=1)] | None = None
    # The model the foreman's own sessions (planning, orchestrating) run on.
    # None: the first claude-code agent's model.
    model: Model | None = None
    # How often the foreman looks at the whole factory and decides what a
    # queue cannot (ORCHESTRATE.md). None: never.
    orchestrate_minutes: Annotated[int, Field(ge=1)] | None = None
    # The watchdog ends a session that runs longer than this. None: never.
    session_minutes: Annotated[int, Field(ge=1)] | None = None


class ReleaseRules(Strict):
    freeze_every_days: Annotated[int, Field(ge=1)]
    uses_required: Annotated[int, Field(ge=0)]
    soak_minutes: Annotated[int, Field(ge=0)]


class Config(Strict):
    """The factory's policies. A preset and a factory version are the same object."""

    version: Annotated[int, Field(ge=1)]
    plans: tuple[Plan, ...]
    agents: tuple[AgentType, ...]
    # Every push needs an approval from each of these on that push to merge.
    # "placeholder" approves every push at once, until a real one is set up.
    reviewers: Annotated[tuple[Slug, ...], Field(min_length=1)]
    foreman: ForemanRules
    release: ReleaseRules
    guesses: Set(str) = frozenset()  # dotted paths no measurement backs yet

    @model_validator(mode="after")
    def _consistent(self) -> Config:
        plans = [p.name for p in self.plans]
        types = [t.type for t in self.agents]
        if len(set(plans)) != len(plans):
            raise ValueError("plan names repeat")
        if len(set(self.reviewers)) != len(self.reviewers):
            raise ValueError("a reviewer is listed twice")
        if len(set(types)) != len(types):
            raise ValueError("an agent type is listed twice")
        for t in self.agents:
            if t.plan not in plans:
                raise ValueError(f"agent type {t.type} names unknown plan {t.plan}")
        for g in self.guesses:
            if not _is_value(self, g.split(".")):
                raise ValueError(f"guess {g!r} names no single value in the config")
        return self

    def type_of(self, agent: str) -> AgentType | None:
        """The type an agent name belongs to, if the config has that agent."""
        return next((t for t in self.agents if agent in t.names()), None)

    def names(self) -> list[str]:
        return [n for t in self.agents for n in t.names()]


def _is_value(obj: object, path: list[str]) -> bool:
    for p in path:
        if isinstance(obj, tuple) and p.isdigit() and int(p) < len(obj):
            obj = obj[int(p)]
        elif isinstance(obj, BaseModel) and p in type(obj).model_fields:
            obj = getattr(obj, p)
        else:
            return False
    return not isinstance(obj, (BaseModel, tuple, frozenset))


# ---------------------------------------------------------------- codes


class Kind(enum.StrEnum):
    initiative = "initiative"
    enhancement = "enhancement"
    bug = "bug"
    friction = "friction"


class ReturnReason(enum.StrEnum):
    blocked = "blocked"  # the holder: it added a dependency that is open
    gave_up = "gave_up"  # the holder: it cannot do the issue as written
    quota = "quota"  # the foreman: the holder is out of quota
    stale = "stale"  # the foreman: the holder's session ended
    preempted = "preempted"  # the foreman: a bug has waited too long
    asked = "asked"  # the holder: it asked the owner on the issue; held until they answer
    no_progress = "no_progress"  # the foreman: the holder's sessions keep ending without moving it


class CiResult(enum.StrEnum):
    passed = "pass"
    failed = "fail"


class ReviewResult(enum.StrEnum):
    approved = "approved"
    changes = "changes"


class ReviewReason(enum.StrEnum):
    incomplete = "incomplete"  # does not do all the issue asks
    wrong = "wrong"  # does something other than the issue asks
    bug = "bug"  # introduces a bug
    tests = "tests"  # the change is not tested
    scope = "scope"  # does more than the issue asks
    other = "other"  # a reviewer whose findings are not coded; see its note


class DivergeReason(enum.StrEnum):
    foreign_push = "foreign_push"  # its head is a commit the factory did not record
    closed = "closed"  # closed on GitHub without a merge


class MergeFailure(enum.StrEnum):
    conflict = "conflict"
    stale_base = "stale_base"


class CloseReason(enum.StrEnum):
    done = "done"  # an initiative whose children are all closed
    duplicate = "duplicate"
    wontfix = "wontfix"
    invalid = "invalid"  # not a bug after all, or cannot be reproduced
    completed = "completed"  # done outside the factory, by the human or an outsider


class SessionEnd(enum.StrEnum):
    finished = "finished"
    crashed = "crashed"
    quota = "quota"
    killed = "killed"


class HoldReason(enum.StrEnum):
    known_bugs = "known_bugs"  # an open bug was found in code or use
    uses = "uses"  # too few power user sessions ended on this build
    soak = "soak"  # too little time since this build, or the last bug found
    stale_build = "stale_build"  # a backport is in no build yet


class Nature(enum.StrEnum):
    """What appending the event means.

    decision: the append is the act. The ledger refusing it means the act did
    not happen, and the actor does something else.
    observation: the act happened outside (on GitHub, in CI). The ledger
    refusing it means the model and the world disagree, which is an alarm.
    """

    decision = "decision"
    observation = "observation"


# ---------------------------------------------------------------- events


class Event(Strict):
    actors: ClassVar[frozenset[str]]
    nature: ClassVar[Nature]
    event: str


D, O = Nature.decision, Nature.observation
H, F, W, PM, PU = Role.human, Role.foreman, Role.watchdog, Role.pm, Role.power_user


def event(actors, nature):
    def mark(cls):
        cls.actors, cls.nature = frozenset(actors), nature
        return cls
    return mark


@event({H}, D)
class ConfigApplied(Event):
    """The first line, and any change of policy after. Versions go up by one.
    It may not drop an agent that is live or holds an issue."""
    event: Literal["config_applied"]
    config: Config


@event({H}, D)
class ScopeFrozen(Event):
    """Once. No enhancement or initiative is filed after it."""
    event: Literal["scope_frozen"]


@event({H, F, PU, AGENT}, O)
class Filed(Event):
    """The issue exists on GitHub, which gave it a new number. parent: an open initiative. deps: open work.

    Bugs and friction are filed by whoever found them, where they found them:
    an agent in the issue it holds, the power user in its open use session,
    the human in a use session or as a plain report whenever it likes. The world records which.
    Initiatives and enhancements come from the human and the foreman; an agent
    may file follow-up enhancements under the initiative of the issue it holds.
    """
    event: Literal["filed"]
    issue: Number
    kind: Kind
    parent: Number | None = None
    deps: Set(Number) = frozenset()

    @model_validator(mode="after")
    def _shape(self) -> Filed:
        if self.kind is Kind.initiative and (self.deps or self.parent):
            raise ValueError("an initiative has no deps or parent")
        if self.issue in self.deps:
            raise ValueError("an issue cannot depend on itself")
        return self


@event({H, F, AGENT}, D)
class Depends(Event):
    """issue waits for on. Both open work issues; no cycle."""
    event: Literal["depends"]
    issue: Number
    on: Number


class ParkReason(enum.StrEnum):
    owner = "owner"  # the owner's hold (on GitHub, the label guard)
    lock = "lock"  # a lock by someone outside the factory (a label wip-NAME)


@event({H, Role.outside}, O)
class Parked(Event):
    """An open issue went on hold. Waiting work is not taken while parked,
    and an initiative is not planned. The owner's hold is the human's; an
    outside lock is an outsider's."""
    event: Literal["parked"]
    issue: Number
    reason: ParkReason = ParkReason.owner  # lines from before locks have none


@event({H, Role.outside}, O)
class Unparked(Event):
    """The hold on a parked issue was lifted. Agents that gave up on it may
    take it again."""
    event: Literal["unparked"]
    issue: Number


@event({H, F, AGENT}, D)
class Undepends(Event):
    """Removes an edge that exists."""
    event: Literal["undepends"]
    issue: Number
    on: Number
    note: Note


@event({H, F, Role.outside, AGENT}, D)
class Closed(Event):
    """Closes an issue without a merge. The foreman closes waiting work and
    initiatives whose children are closed. An agent closes an issue it holds
    and has no open pull request for as completed, when it found nothing left
    to change, with the evidence in the note. The human and outsiders close
    whatever is open (on GitHub): an agent holding it loses it, and its open
    pull request is abandoned."""
    event: Literal["closed"]
    issue: Number
    reason: CloseReason
    duplicate_of: Number | None = None
    note: Note | None = None

    @model_validator(mode="after")
    def _dup(self) -> Closed:
        if (self.reason is CloseReason.duplicate) != (self.duplicate_of is not None):
            raise ValueError("duplicate_of goes with reason duplicate, and only then")
        if self.duplicate_of == self.issue:
            raise ValueError("an issue is not its own duplicate")
        return self


@event({W}, O)
class SessionStarted(Event):
    """An off agent with quota starts; it resumes the issue it still holds."""
    event: Literal["session_started"]
    agent: AgentName


@event({W}, O)
class SessionEnded(Event):
    """A live agent stops. Its issue stays held until the foreman returns it."""
    event: Literal["session_ended"]
    agent: AgentName
    reason: SessionEnd


@event({W}, O)
class QuotaOut(Event):
    event: Literal["quota_out"]
    agent: AgentName


@event({W}, O)
class QuotaBack(Event):
    event: Literal["quota_back"]
    agent: AgentName


@event({AGENT}, D)
class Taken(Event):
    """An idle agent with quota takes a ready issue it has not given up on.
    This append is the lock."""
    event: Literal["taken"]
    issue: Number


@event({F, AGENT}, D)
class Returned(Event):
    """A held issue goes back to waiting. The reason says who may return it."""
    event: Literal["returned"]
    issue: Number
    reason: ReturnReason
    note: Note | None = None


@event({AGENT}, O)
class Push(Event):
    """The holder pushed; this ends its hold. The first push opens a new pull
    request number; pushes go 1, 2, 3. Open checks of the push before end."""
    event: Literal["push"]
    issue: Number
    pr: Number
    push: PushNo
    sha: Sha


@event({Role.ci}, O)
class CiDone(Event):
    """On the latest push, while pending. fail voids the review and the issue
    needs work."""
    event: Literal["ci_done"]
    pr: Number
    push: PushNo
    result: CiResult


@event({Role.reviewer}, O)
class ReviewDone(Event):
    """One reviewer of the config on the latest push, while pending. changes
    need reasons, and the issue needs work."""
    event: Literal["review_done"]
    by: Slug
    pr: Number
    push: PushNo
    result: ReviewResult
    reasons: Set(ReviewReason) = frozenset()
    note: Note | None = None

    @model_validator(mode="after")
    def _reasons(self) -> ReviewDone:
        if (self.result is ReviewResult.changes) != bool(self.reasons):
            raise ValueError("changes need reasons; an approval has none")
        return self


@event({Role.merger}, O)
class Diverged(Event):
    """The pull request changed on GitHub outside the factory, while its
    issue was in checks or its pull request open: the latest push's pending
    checks void, and an issue in checks waits again. foreign_push: someone
    pushed to its branch, so the next agent starts from their commit. closed:
    the pull request is abandoned; the next push opens a new one."""
    event: Literal["diverged"]
    pr: Number
    push: PushNo
    reason: DivergeReason


@event({Role.merger}, O)
class MergeFailed(Event):
    """The latest push is green and approved but did not merge: needs work."""
    event: Literal["merge_failed"]
    pr: Number
    push: PushNo
    reason: MergeFailure


@event({Role.merger, H, Role.outside}, O)
class Merged(Event):
    """The pull request merged and its issue closes. By the merger: the latest
    push is green and approved by every reviewer. By the human or an outsider
    on GitHub: whatever its checks say, and an agent holding the issue loses
    it."""
    event: Literal["merged"]
    pr: Number
    push: PushNo
    sha: Sha  # the commit on main


@event({H, F, Role.reviewer, AGENT}, D)
class Attributed(Event):
    """bug came in with merged pr. A second attribution of the same bug must
    name the first in replaces."""
    event: Literal["attributed"]
    bug: Number
    pr: Number
    replaces: Seq | None = None


@event({F}, D)
class TickStarted(Event):
    event: Literal["tick_started"]
    tick: Number


@event({F}, D)
class TickEnded(Event):
    event: Literal["tick_ended"]
    tick: Number


@event({PM}, D)
class Frozen(Event):
    """A release branch for a version above every earlier one."""
    event: Literal["frozen"]
    version: Version
    sha: Sha  # the commit on main it was cut from


@event({PM}, O)
class Built(Event):
    """Candidate build n of a candidate version; n goes 1, 2, 3."""
    event: Literal["built"]
    version: Version
    build: Number
    sha: Sha


@event({PM}, D)
class Backported(Event):
    """A bug fix merged after the freeze enters the version's branch."""
    event: Literal["backported"]
    version: Version
    pr: Number


@event({H, PU}, O)
class UseStarted(Event):
    """One session at a time each, on a version's latest build: a candidate
    for the power user, a published release for the human."""
    event: Literal["use_started"]
    use: Number
    version: Version
    build: Number


@event({H, PU}, O)
class UseEnded(Event):
    event: Literal["use_ended"]
    use: Number


@event({PM}, D)
class ReleaseHeld(Event):
    """The reasons are exactly those true of the latest build."""
    event: Literal["release_held"]
    version: Version
    build: Number
    reasons: Annotated[Set(HoldReason), Field(min_length=1)]


@event({PM}, D)
class Published(Event):
    """The latest build, with no hold reason true of it."""
    event: Literal["published"]
    version: Version
    build: Number


EVENTS: tuple[type[Event], ...] = tuple(
    c for c in list(globals().values())
    if isinstance(c, type) and issubclass(c, Event) and c is not Event)
Body = Annotated[Union[EVENTS], Field(discriminator="event")]  # type: ignore[valid-type]
BODY = TypeAdapter(Body)


# ---------------------------------------------------------------- lines


class Envelope(Strict):
    schema_: Literal[1] = Field(alias="schema")
    seq: Seq
    t: Time
    actor: Actor


ENVELOPE = frozenset(f.alias or n for n, f in Envelope.model_fields.items())
assert not any(ENVELOPE & set(c.model_fields) for c in EVENTS)


@dataclass(frozen=True)
class Line:
    seq: int
    t: datetime
    actor: str
    body: Event

    def __post_init__(self):
        assert self.t.tzinfo is timezone.utc

    def dump(self) -> str:
        env = {"schema": SCHEMA, "seq": self.seq,
               "t": self.t.strftime("%Y-%m-%dT%H:%M:%SZ"), "actor": self.actor}
        return json.dumps(env | self.body.model_dump(mode="json", exclude_defaults=True),
                          separators=(",", ":"))


def _object(text: str) -> dict:
    def no_dup(pairs):
        keys = [k for k, _ in pairs]
        if len(set(keys)) != len(keys):
            raise ValueError("a key repeats")
        return dict(pairs)
    d = json.loads(text, object_pairs_hook=no_dup)
    if not isinstance(d, dict):
        raise ValueError("a line is one JSON object")
    return d


def parse_body(text: str) -> Event:
    return BODY.validate_json(json.dumps(_object(text)), strict=True)


def parse(text: str) -> Line:
    d = _object(text)
    env = Envelope.model_validate_json(json.dumps({k: d.pop(k) for k in ENVELOPE if k in d}))
    body = BODY.validate_json(json.dumps(d), strict=True)
    return Line(env.seq, env.t, env.actor, body)
