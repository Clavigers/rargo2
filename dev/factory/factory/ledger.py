"""The ledger: the one writer of the factory log.

An actor submits a body (the event and its fields). The ledger, holding an
exclusive lock on the log file, catches up on lines other processes wrote,
stamps the next seq and the time, checks the line against the world, and only
then appends and fsyncs it. So the append is the act: two agents racing to
take one issue both submit, and the ledger accepts exactly one.

A refusal says what kind of event was refused (model.Nature):
- Refused: a decision. It did not happen; the actor does something else.
- Alarm: an observation. Something happened outside that the model says
  cannot; a person should look.

Crash safety: a line is acknowledged only after fsync. A writer that died
mid-write leaves a tail with no newline; it was never acknowledged, so the
next writer cuts it off before appending.
"""
from __future__ import annotations

import contextlib
import fcntl
import os
from datetime import datetime, timezone
from typing import Callable

from . import model as m
from .world import Illegal, World


class Refused(Exception):
    """A decision the world does not allow. Nothing happened."""

    def __init__(self, event: str, msg: str):
        super().__init__(f"{event} refused: {msg}")
        self.event, self.msg = event, msg


class Alarm(Exception):
    """An observation the world says is impossible. The model and the world
    disagree."""

    def __init__(self, event: str, msg: str):
        super().__init__(f"{event} contradicts the model: {msg}")
        self.event, self.msg = event, msg


def utc_now() -> datetime:
    return datetime.now(timezone.utc).replace(microsecond=0)


class Ledger:
    def __init__(self, path: str | os.PathLike, clock: Callable[[], datetime] = utc_now):
        self.path = os.fspath(path)
        self.clock = clock
        self.world = World()
        self._offset = 0
        with self._locked(fcntl.LOCK_SH) as fd:
            self._catch_up(fd, repair=False)

    def submit(self, actor: str, body: m.Event | str, dry: bool = False) -> m.Line:
        """Append one event for actor, or raise Refused, Alarm or ValueError
        (malformed body). Returns the line as written. dry: check it against
        the world as it is now and write nothing, so an actor can ask before
        an act it will then record as an observation."""
        if isinstance(body, str):
            body = m.parse_body(body)
        with self._locked(fcntl.LOCK_EX) as fd:
            self._catch_up(fd, repair=True)
            now = self.clock()
            if now.tzinfo is None:
                raise ValueError("the clock must give aware times")
            t = now.astimezone(timezone.utc).replace(microsecond=0)
            if self.world.t and t < self.world.t:
                t = self.world.t  # the log's time never runs backwards
            line = m.Line(self.world.seq + 1, t, actor, body)
            try:
                nxt = self.world.attempt(line)
            except Illegal as err:
                kind = Refused if body.nature is m.Nature.decision else Alarm
                raise kind(body.event, err.msg) from None
            if dry:
                return line
            data = (line.dump() + "\n").encode()
            if os.write(fd, data) != len(data):
                raise OSError("short write to the log")
            os.fsync(fd)
            self._offset += len(data)
            self.world = nxt
        return line

    def refresh(self) -> World:
        """The world as of the last line any process wrote."""
        with self._locked(fcntl.LOCK_SH) as fd:
            self._catch_up(fd, repair=False)
        return self.world

    # ------------------------------------------------------------ inside the lock

    @contextlib.contextmanager
    def _locked(self, how: int):
        fd = os.open(self.path, os.O_RDWR | os.O_CREAT | os.O_APPEND, 0o644)
        try:
            fcntl.flock(fd, how)
            yield fd
        finally:
            os.close(fd)  # releases the lock

    def _catch_up(self, fd: int, repair: bool) -> None:
        """Apply the lines written since we last looked. Readers and writers
        exclude each other, so a tail with no newline is a dead writer's
        unacknowledged line: a writer cuts it off, a reader leaves it."""
        size = os.fstat(fd).st_size
        rest = os.pread(fd, size - self._offset, self._offset)
        end = rest.rfind(b"\n") + 1  # bytes up to the last complete line
        if repair and end < len(rest):
            os.ftruncate(fd, self._offset + end)
        for text in rest[:end].splitlines():
            # Each line was checked when it was written; the fast path will do.
            self.world.apply(m.parse(text.decode()))
        self._offset += end
