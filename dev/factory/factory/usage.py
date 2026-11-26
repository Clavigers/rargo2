"""How much of each agent plan's quota is used, and when it resets.

    python -m factory usage STATE [--every SECONDS]

Each reading that differs from the last goes into the log as quota_seen, so
the history of every plan's quota is there with everything else. The last
reading, with any reader's error, is also in STATE/usage.json. Nothing in
the factory decides from it.

How each is read, none of them a documented interface:
- claude-code: the interactive `/usage` screen, in a hidden tmux window.
- grok: the same, its `/usage` screen's "Usage limit" tab.
- codex: the rate_limits that `codex exec` writes into its newest session file.
The screens change between releases; a reading that fails says so in "error"
and the rest still update.
"""
from __future__ import annotations

import json
import os
import re
import subprocess
import time
from datetime import datetime, timedelta, timezone
from pathlib import Path
from zoneinfo import ZoneInfo

LOCAL = ZoneInfo(os.environ.get("FACTORY_TZ", "Europe/Berlin"))


def iso(t: datetime) -> str:
    return t.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def screen(argv: list[str], cwd: Path, keys: list[list[str]], wait: float = 8.0, env=None) -> str:
    """Start argv in a hidden tmux window, send keys, return the screen."""
    name = f"usage-{os.getpid()}"
    subprocess.run(["tmux", "kill-session", "-t", name], capture_output=True)
    e = {**os.environ, **(env or {})}
    subprocess.run(["tmux", "new-session", "-d", "-s", name, "-x", "200", "-y", "80", *argv],
                   cwd=cwd, env=e, check=True)
    try:
        time.sleep(wait)
        for k in keys:
            if k == ["TRUST?"]:  # a folder trust prompt, the first time
                out = subprocess.run(["tmux", "capture-pane", "-p", "-t", name], capture_output=True, text=True).stdout
                if "trust this folder" in out:
                    subprocess.run(["tmux", "send-keys", "-t", name, "Down", "Enter"])
                    time.sleep(wait)
                continue
            subprocess.run(["tmux", "send-keys", "-t", name, *k])
            time.sleep(1.5)
        time.sleep(wait)
        return subprocess.run(["tmux", "capture-pane", "-p", "-t", name], capture_output=True, text=True).stdout
    finally:
        subprocess.run(["tmux", "kill-session", "-t", name], capture_output=True)


def when(text: str, now: datetime) -> str | None:
    """A reset time as a screen prints it, in local time: "1:09pm",
    "Nov 28, 5:59am", "November 30, 19:47"."""
    text = re.sub(r"\s*\(.*?\)\s*$", "", text.strip())
    local = now.astimezone(LOCAL)
    for fmt in ("%b %d, %I:%M%p", "%B %d, %I:%M%p", "%b %d, %H:%M", "%B %d, %H:%M",
                "%b %d, %I%p", "%B %d, %I%p"):
        try:
            d = datetime.strptime(text, fmt).replace(year=local.year, tzinfo=LOCAL)
            if d < local - timedelta(days=1):  # December's screen naming January
                d = d.replace(year=local.year + 1)
            return iso(d)
        except ValueError:
            pass
    for fmt in ("%I:%M%p", "%I%p", "%H:%M"):
        try:
            t = datetime.strptime(text, fmt)
            d = local.replace(hour=t.hour, minute=t.minute, second=0, microsecond=0)
            if d < local:
                d += timedelta(days=1)
            return iso(d)
        except ValueError:
            pass
    return None


def parse_claude(text: str, now: datetime) -> list[dict]:
    """Blocks of: a title line, a bar line ending "N% used", "Resets ..."."""
    lines = [l.strip() for l in text.splitlines() if l.strip()]
    out = []
    for k, l in enumerate(lines):
        m = re.search(r"(\d+(?:\.\d+)?)% used$", l)
        if not m or k == 0:
            continue
        title = lines[k - 1]
        if title.startswith(("$", "Usage credits")) or "%" in title:
            title = "Usage credits" if "credits" in title.lower() else title
        r = lines[k + 1] if k + 1 < len(lines) else ""
        resets = when(r.removeprefix("Resets").strip(), now) if r.startswith("Resets") else None
        if r.startswith("$"):  # credits: "$0 / $100 spent · Resets Dec 1 (...)"
            mm = re.search(r"Resets (.*)$", r)
            resets = when(re.sub(r"\s*\(.*?\)", "", mm.group(1)) + ", 12:00am", now) if mm else None
        out.append(dict(name=title, used=float(m.group(1)), resets=resets))
    return out


def parse_grok(text: str, now: datetime) -> list[dict]:
    rows = [re.sub(r"^.*?│\s*|\s*│\s*$", "", l).strip() for l in text.splitlines()]
    rows = [r for r in rows if r]
    out = []
    for k, r in enumerate(rows):
        m = re.search(r"(\d+(?:\.\d+)?)%\s*$", r)
        if m and k and ("█" in r or "░" in r):
            resets = None
            for nxt in rows[k + 1:k + 3]:
                if nxt.startswith("Resets"):
                    resets = when(nxt.split(":", 1)[1], now)
            out.append(dict(name=rows[k - 1], used=float(m.group(1)), resets=resets))
    if not out:  # the prompt border also says "Weekly limit left: N%"
        m = re.search(r"Weekly limit left:\s*(\d+)%", text)
        if m:
            out.append(dict(name="Weekly limit", used=100.0 - float(m.group(1)), resets=None))
    return out


def codex(home: Path) -> list[dict]:
    files = sorted((home / ".codex" / "sessions").rglob("*.jsonl"), key=lambda p: p.stat().st_mtime)
    for f in reversed(files[-20:]):
        last = None
        for line in f.read_text(errors="replace").splitlines():
            if '"rate_limits"' in line:
                last = line
        if last is None:
            continue
        rl = _find(json.loads(last), "rate_limits")
        out = []
        for key, name in (("primary", "5-hour window"), ("secondary", "Week")):
            w = rl.get(key) if rl else None
            if w:
                mins = w.get("window_minutes")
                if mins and mins != 300 and key == "primary":
                    name = f"{mins // 60}-hour window"
                out.append(dict(name=name, used=float(w["used_percent"]),
                                resets=iso(datetime.fromtimestamp(w["resets_at"], timezone.utc))))
        return out
    return []


def _find(d, key):
    if isinstance(d, dict):
        if key in d:
            return d[key]
        for v in d.values():
            r = _find(v, key)
            if r is not None:
                return r
    elif isinstance(d, list):
        for v in d:
            r = _find(v, key)
            if r is not None:
                return r
    return None


def probe(state: Path) -> dict:
    now = datetime.now(timezone.utc)
    work = state / "usage"
    work.mkdir(parents=True, exist_ok=True)
    plans = {}
    readers = {
        "claude-code": lambda: parse_claude(screen(["claude"], work, [["TRUST?"], ["/usage"], ["Enter"]],
                                                   env={"IS_SANDBOX": "1"}), now),
        "grok": lambda: parse_grok(screen([os.environ.get("FACTORY_GROK", "grok")], work,
                                          [["/usage"], ["Enter"]]), now),
        "codex": lambda: codex(Path.home()),
    }
    for name, read in readers.items():
        try:
            ws = read()
            plans[name] = dict(windows=ws, error=None if ws else "nothing read from its screen")
        except Exception as err:  # one failing reader leaves the others
            plans[name] = dict(windows=[], error=f"{type(err).__name__}: {err}")
    return dict(t=iso(now), plans=plans)


def record(out: dict, log: str, say=print) -> None:
    """Write each plan's reading into the log, if it changed."""
    from .ledger import Alarm, Ledger, Refused
    from .model import Role
    led = Ledger(log)
    for harness, plan in out["plans"].items():
        if not plan["windows"]:
            continue
        last = led.refresh().quota_seen.get(harness)
        if last and [w.model_dump(mode="json") for w in last[1]] == plan["windows"]:
            continue
        try:
            led.submit(Role.watchdog, json.dumps(
                {"event": "quota_seen", "harness": harness, "windows": plan["windows"]}))
        except (Alarm, Refused, ValueError) as err:
            say(f"usage: {harness}: {err}")


def main(argv: list[str]) -> int:
    state = Path(argv[0])
    every = float(argv[argv.index("--every") + 1]) if "--every" in argv else 0
    while True:
        out = probe(state)
        if os.environ.get("FACTORY_LOG"):
            record(out, os.environ["FACTORY_LOG"])
        tmp = state / "usage.json.tmp"
        tmp.write_text(json.dumps(out, indent=1))
        tmp.replace(state / "usage.json")
        print(json.dumps(out), flush=True)
        if not every:
            return 0
        time.sleep(every)
