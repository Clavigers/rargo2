"""The power user: it uses each release candidate the way a person would,
and files what goes wrong.

Every few minutes it looks for a use that is due: a candidate whose latest
build has had fewer power user uses end on it than the config's
uses_required, with no backport waiting for a build. With
`release.use_beyond_required` it also goes on using a candidate's latest
build when no build needs a use. Then it

1. records `use_started` for that build;
2. checks the build's commit out in CHECKOUT and runs harness sessions there
   with POWER-USER.md as their prompt, one after another, until the use has
   lasted the config's `release.use_minutes` (one session when that is not
   set), a session fails, or the build is no longer the latest. A session
   ends when its model says it is done, long before a person would stop, so
   one session is a few minutes of use: the time comes from running many.
   Each is given one area to concentrate on (FOCUS, in turn) and what the
   sessions before it reported, so it goes where they did not. A session
   builds the candidate, uses it, and files bugs and friction on GitHub; the
   observer records them as found in this use;
3. waits until the observer has recorded every issue the sessions filed (it
   finds them on GitHub by the use's line in their bodies), and records
   `use_ended`.

Sessions are at least the config's `release.use_gap_minutes` apart (three
hours when it is not set), so a week of small backports does not become a
week of sessions unless the config asks for that. A session that did not
exit cleanly (a crash, no quota, the time limit), or that ended within
MIN_SESSION (a harness that only printed that it may not run), is a failed
session. A use none of whose sessions succeeded ends as void: it is not a
use of the build, and another is tried after `retry`, or after the gap when
that is shorter. So is a use left open by a power user
that stopped, which the next one ends. It writes no code and touches no pull
request.

Each session runs in its own session (process group) with a directory the
factory made for it, which it is told to work under (and is its TMPDIR).
Whatever the session ended like, the factory then kills every process left
of it (those in its session, or that carry its mark in their environment,
or whose working directory or command line is in its directory), unmounts
every mount under that directory, lazily if it must, and removes it, and
says what it had to kill and unmount. It reaches nothing outside: not the
shared server, not /root/.cache/rargo, not another session's directory.
"""
from __future__ import annotations

import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timedelta
from pathlib import Path
from typing import Callable

from . import harness
from . import machines as M
from . import model as m
from .github import GitHubError
from .ledger import Alarm, Ledger, Refused, utc_now
from .model import Role
from .watchdog import BIN

GAP = timedelta(hours=3)  # between uses, when the config's release.use_gap_minutes is not set
MIN_SESSION = timedelta(minutes=2)  # a session over sooner than this did not use anything
MAX_SESSIONS = 40  # in one use, whatever its budget
NOTES = 3  # earlier sessions whose closing reports the next one is given
NOTE_CHARS = 3000

# What a session concentrates on. Each session takes the next one, across uses.
FOCUS = (
    ("the everyday path",
     "build, check, test, run, an edit and a rebuild, a clean cache and a warm one, on this "
     "repository and on two or three small crates you write or clone"),
    ("what changed in this version",
     "read `git log --oneline $(git describe --tags --abbrev=0 HEAD^)..HEAD` and try every change a "
     "user can reach, the way its commit message says it behaves and the ways it does not mention"),
    ("rough handling",
     "interrupt builds and start again, run several builds at once in one project and in two, kill "
     "your server mid-build and bring it back, fill or remove its state directory, give wrong arguments"),
    ("errors a user will read",
     "break code, manifests, lockfiles, dependencies and toolchain files in the ways people do, and "
     "judge whether each message says what went wrong and what to do"),
    ("the other cargo commands",
     "clippy, doc, bench, install, nextest, run with arguments, examples, workspaces with several "
     "members, features, build scripts, proc macros, and what an editor's rust-analyzer runs"),
    ("several clients and sign-in",
     "tokens and `rargo auth`, two clients with separate caches building the same and different "
     "projects against your one server, a client on an older checkout, UDP and TCP links"),
    ("projects unlike this one",
     "clone a few real crates from crates.io or GitHub of different shapes (a big dependency tree, "
     "a -sys crate, a project with its own flake.nix or rust-toolchain file) and work in them"),
    ("speed against plain cargo",
     "time cold and warm builds, a one-line edit, a no-op rebuild and a test run with rargo and with "
     "cargo on the same projects; anything slower than cargo where it should not be is a problem"),
    ("the documentation as a new user",
     "follow README.md, the Book and the Reference word for word on a clean setup, and file every "
     "step that does not work as written or leaves a new user stuck"),
)


class PowerUser:
    def __init__(self, ledger: Ledger, gh, checkout: Path, prompt: str,
                 clock: Callable[[], datetime] = utc_now,
                 gap: timedelta | None = None, retry: timedelta = timedelta(minutes=30),
                 timeout: timedelta = timedelta(minutes=90),
                 settle: timedelta = timedelta(minutes=20), poll: float = 20.0,
                 run: Callable[..., int] | None = None,
                 prepare: Callable[[str], None] | None = None,
                 say: Callable[[str], None] = print,
                 scratch: Path | None = None,
                 mounts: Callable[[], list[str]] | None = None,
                 unmount: Callable[[str, bool], bool] | None = None):
        self.ledger, self.gh, self.checkout, self.prompt = ledger, gh, Path(checkout), prompt
        self.clock, self.gap, self.retry, self.timeout = clock, gap, retry, timeout
        self.settle, self.poll = settle, poll
        self.round = 1  # which session of the use is running
        self.run_session = run or self._run_session
        self.prepare = prepare or (self._checkout if run is None else lambda sha: None)
        self.say = say
        self.scratch = Path(scratch or tempfile.gettempdir())  # where session directories are made
        self.mounts = mounts or _mounts
        self.unmount = unmount or _unmount

    def run(self, every: float = 300.0) -> None:
        while True:
            try:
                self.step()
            except (subprocess.CalledProcessError, GitHubError, Alarm, Refused) as err:
                self.say(f"power user: {err}")  # the next step tries again
            time.sleep(every)

    def due(self) -> tuple[str, int, str] | None:
        """The version, build and commit to use now, if any."""
        w = self.ledger.refresh()
        if w.config is None:
            return None
        mine = {n: u for n, u in w.uses.items() if u.by == Role.power_user}
        gap = self.gap  # set only by a test; the config's otherwise
        if gap is None:
            minutes = w.config.release.use_gap_minutes
            gap = GAP if minutes is None else timedelta(minutes=minutes)
        last = max(mine, default=None)  # after a session that failed, try again sooner
        if last and self.clock() - w.since(M.USE, last) < (min(self.retry, gap) if mine[last].void else gap):
            return None
        for version, v in w.versions.items():
            build = len(v.builds)
            if not w.is_(M.VERSION, version, "candidate") or build == 0 or v.unbuilt:
                continue
            ended = sum(1 for n, u in mine.items() if (u.version, u.build) == (version, build)
                        and w.is_(M.USE, n, "ended") and not u.void)
            if ended < w.config.release.uses_required:
                return version, build, self._sha(version)
        if w.config.release.use_beyond_required:
            for version, v in w.versions.items():
                if w.is_(M.VERSION, version, "candidate") and v.builds and not v.unbuilt:
                    return version, len(v.builds), self._sha(version)
        return None

    def _sha(self, version: str) -> str:
        return [l["sha"] for l in self._lines("built") if l["version"] == version][-1]

    def _more(self, version: str, build: int, began: datetime) -> bool:
        """Whether the use goes on to another session."""
        w = self.ledger.refresh()
        budget, v = w.config.release.use_minutes, w.versions[version]
        return (budget is not None and self.round < MAX_SESSIONS
                and self.clock() - began < timedelta(minutes=budget)
                and w.is_(M.VERSION, version, "candidate") and len(v.builds) == build and not v.unbuilt)

    def step(self) -> None:
        w = self.ledger.refresh()
        for n, u in w.uses.items():  # left open by a power user that stopped
            if u.by == Role.power_user and w.is_(M.USE, n, "open"):
                self._record("use_ended", use=n, void=True)
        d = self.due()
        if d is None:
            return
        version, build, sha = d
        self.prepare(sha)  # before the use opens: a checkout that fails is no session
        use = max(self.ledger.world.uses, default=0) + 1
        began = self.clock()
        self._record("use_started", use=use, version=version, build=build)
        self.say(f"power user: use {use}: {version} build {build} at {sha[:7]}")
        codes: list[int] = []
        try:
            for self.round in range(1, MAX_SESSIONS + 1):
                codes.append(self.run_session(use, version, build, sha))
                self.say(f"power user: use {use} session {self.round} ended with exit {codes[-1]}")
                if codes[-1] != 0 or not self._more(version, build, began):
                    break
            self._settle(use, began)
        finally:
            self._record("use_ended", use=use, void=0 not in codes)

    def _settle(self, use: int, began: datetime) -> None:
        """Wait until the log has every issue this session filed: they are
        found in this use only while it is open."""
        mark = f"use session {use}."
        filed = {i["number"] for i in self.gh.issues(began - timedelta(minutes=5))
                 if mark in (i.get("body") or "") and "factory-actor: power_user" in i["body"]}
        until = self.clock() + self.settle
        while filed - set(self.ledger.refresh().issues) and self.clock() < until:
            time.sleep(self.poll)
        if missing := filed - set(self.ledger.world.issues):
            self.say(f"power user: use {use}: the log never got {sorted(missing)}")
        self.say(f"power user: use {use} filed {sorted(filed) or 'nothing'}")

    def _checkout(self, sha: str) -> None:
        git = ["git", "-C", str(self.checkout)]
        subprocess.run([*git, "fetch", "-q", "--tags", "--force", "origin"], check=True, capture_output=True)
        subprocess.run([*git, "checkout", "-q", "--detach", "-f", sha], check=True, capture_output=True)

    def _run_session(self, use: int, version: str, build: int, sha: str) -> int:
        w = self.ledger.world
        t = next((a for a in w.config.agents if a.type is m.Harness.claude_code), w.config.agents[0])
        if w.config.release.use_model is not None:
            t = t.model_copy(update={"model": w.config.release.use_model})
        log = self.checkout.parent / "state" / "logs"
        log.mkdir(parents=True, exist_ok=True)
        before = sorted(log.glob("use-*.log"), key=lambda p: p.stat().st_mtime)
        area, how = FOCUS[len(before) % len(FOCUS)]
        notes = "\n\n".join(
            f"--- an earlier session ended with:\n{text[-NOTE_CHARS:]}"
            for p in before[-NOTES:] if (text := p.read_text(errors="replace").strip()))
        self.scratch.mkdir(parents=True, exist_ok=True)
        work = Path(os.path.realpath(tempfile.mkdtemp(prefix=f"pu{use}.{self.round}.", dir=self.scratch)))
        proc = None
        try:
            prompt = (self.prompt.replace("{dir}", str(work))
                      .replace("{version}", version).replace("{build}", str(build))
                      .replace("{sha}", sha).replace("{use}", str(use))
                      .replace("{session}", str(self.round)).replace("{focus}", f"{area}: {how}")
                      .replace("{notes}", notes or "None: this is the first session."))
            cmd = harness.command(t, prompt)
            if cmd is None:
                self.say(f"power user: no {t.type} here to run a session with")
                return 127
            argv, extra = cmd
            env = os.environ | extra | {
                "TMPDIR": str(work), MARK: str(work),
                "FACTORY_LOG": self.ledger.path, "FACTORY_ACTOR": Role.power_user,
                "FACTORY_PYTHON": sys.executable,
                "PATH": f"{BIN}{os.pathsep}{os.environ.get('PATH', '')}"}
            started = time.monotonic()
            with open(log / f"use-{use}-{self.round}.log", "wb") as out:
                # its own session, so that what it starts is found by its session id too
                proc = subprocess.Popen(argv, cwd=self.checkout, env=env, stdin=subprocess.DEVNULL,
                                        stdout=out, stderr=subprocess.STDOUT, start_new_session=True)
                try:
                    code = proc.wait(timeout=self.timeout.total_seconds())
                except subprocess.TimeoutExpired:
                    return 124
                finally:
                    self.clean(work, use, sid=proc.pid)
                    proc.wait()
        finally:
            if proc is None:  # no session ran: only the directory to remove
                self.clean(work, use)
        if code == 0 and time.monotonic() - started < MIN_SESSION.total_seconds():
            self.say(f"power user: use {use} session {self.round} ended within "
                     f"{MIN_SESSION.seconds // 60} minutes: not a session")
            return 75  # EX_TEMPFAIL: a harness that exits 0 saying it is out of quota
        return code

    def clean(self, work: Path, use: int, sid: int | None = None) -> None:
        """Kill what is left of the session that worked in WORK (and whose
        session id is SID), unmount what it mounted there and remove it,
        saying what was left."""
        work = Path(os.path.realpath(work))
        if not work.exists():
            return
        killed = _kill(work, sid)
        unmounted, stuck = [], []
        for _ in range(3):  # a lazy unmount can uncover a mount beneath it
            here = sorted((p for p in self.mounts() if _under(p, work)), key=len, reverse=True)
            if not here:
                break
            for p in here:
                if self.unmount(p, False) or self.unmount(p, True):
                    unmounted.append(p)
                elif p not in stuck:
                    stuck.append(p)
        stuck = [p for p in self.mounts() if _under(p, work)]
        size = _size(work)
        if stuck:  # removing would reach into what is mounted there
            self.say(f"power user: use {use} session {self.round}: could not unmount {stuck}: "
                     f"left {work} in place")
        else:
            shutil.rmtree(work, ignore_errors=True)
        if killed or unmounted or size:
            what = [f"killed {len(killed)}: " + "; ".join(killed)] if killed else []
            what += [f"unmounted {len(unmounted)}: " + " ".join(unmounted)] if unmounted else []
            what += [f"removed {work} ({size} bytes)"] if not stuck else []
            self.say(f"power user: use {use} session {self.round} left behind: " + "; ".join(what))

    def _record(self, event: str, **fields) -> None:
        self.ledger.submit(Role.power_user, json.dumps({"event": event, **fields}))

    def _lines(self, event: str) -> list[dict]:
        key = f'"event":"{event}"'
        with open(self.ledger.path) as f:
            return [json.loads(l) for l in f if key in l]


MARK = "FACTORY_USE_DIR"  # in the environment of everything a session starts


def _under(path: str, work: Path) -> bool:
    return path == str(work) or path.startswith(str(work) + "/")


def _ancestors() -> set[int]:
    """This process and those above it: never killed, whatever they carry."""
    pids, pid = set(), os.getpid()
    while pid > 1 and pid not in pids:
        pids.add(pid)
        try:
            pid = int(Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[1])
        except (OSError, ValueError, IndexError):
            break
    return pids


def _theirs(work: Path, sid: int | None) -> dict[int, str]:
    """The processes left of the session that worked in WORK, with their
    command lines: in its session SID, or with its mark in their environment,
    their working directory in WORK, or WORK on their command line."""
    found, mark, mine = {}, f"{MARK}={work}".encode(), _ancestors()
    named = re.compile(re.escape(str(work)) + r"(/|\s|$)")
    for d in Path("/proc").iterdir():
        if not d.name.isdigit() or int(d.name) in mine:
            continue
        try:
            stat = (d / "stat").read_text().rsplit(")", 1)[1].split()
            if stat[0] in "ZX":
                continue  # dead already, only not yet reaped
            cmd = (d / "cmdline").read_bytes().replace(b"\0", b" ").decode(errors="replace").strip()
            hit = int(stat[3]) == sid or named.search(cmd) is not None
            for look in (lambda: mark in (d / "environ").read_bytes().split(b"\0"),
                         lambda: _under(os.readlink(d / "cwd"), work)):
                try:
                    hit = hit or look()
                except OSError:
                    pass
            if hit:
                found[int(d.name)] = cmd or f"[{(d / 'comm').read_text().strip()}]"
        except (OSError, ValueError, IndexError):
            continue  # gone, or not ours to read
    return found


def _kill(work: Path, sid: int | None) -> list[str]:
    """Kill everything left of the session, until none is left: what is
    killed can have started more."""
    killed: dict[int, str] = {}
    deadline = time.monotonic() + 30
    while (left := _theirs(work, sid)) and time.monotonic() < deadline:
        for pid, cmd in left.items():
            try:
                os.kill(pid, signal.SIGKILL)
                killed.setdefault(pid, cmd)
            except ProcessLookupError:
                pass
        time.sleep(0.05)
    return [f"{pid} {cmd[:200]}" for pid, cmd in killed.items()]


def _mounts() -> list[str]:
    def unescape(p: str) -> str:  # mountinfo writes space, tab, newline and backslash in octal
        return p.replace("\\040", " ").replace("\\011", "\t").replace("\\012", "\n").replace("\\134", "\\")
    with open("/proc/self/mountinfo") as f:
        return [unescape(l.split()[4]) for l in f if len(l.split()) > 4]


def _unmount(path: str, lazy: bool) -> bool:
    return subprocess.run(["umount", *(["-l"] if lazy else []), path],
                          capture_output=True).returncode == 0


def _size(work: Path) -> int:
    total = 0
    for root, dirs, files in os.walk(work):
        for name in files:
            try:
                total += os.lstat(os.path.join(root, name)).st_size
            except OSError:
                pass
    return total
