"""Serves the factory visualizer: the real factory's log, or the simulation,
drawn the same way.

    python -m viz.server [--log PATH] [--port 8377] [--host 127.0.0.1]

GET  /             the page
GET  /api/params   the simulation's parameters
GET  /api/series   hourly history for the charts
GET  /api/stream   server-sent events: one snapshot per frame
POST /api/control  {"source": "real"|"sim", "action": "play"|"pause"|"reset"|"live",
                    "speed": n, "seed": n, "params": {...}}

Real: reads the log as it grows ("live"), or replays it from the start at a
speed. Sim: viz.simfactory, which writes the same events through the same
checker. Listens on localhost only: reach it over ssh -L.
"""
from __future__ import annotations

import argparse
import json
import os
import pathlib
import threading
import time
from datetime import datetime
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from factory import model as m
from factory.world import World

from . import simfactory
from .view import History, snapshot

HERE = pathlib.Path(__file__).parent


def parse_t(s: str) -> datetime:
    return datetime.fromisoformat(s.replace("Z", "+00:00"))


class Real:
    """The real log. live: everything written so far, followed as it grows.
    replay: from the first line, at speed simulated minutes per second."""
    kind = "real"

    def __init__(self, path: str):
        self.path = path
        self.raw: list[str] = []
        self._offset = 0
        self.live = True
        self._restart()

    def _restart(self):
        self.w = World()
        self.lines: list[dict] = []
        self.history = History()
        self.i = 0
        self.cursor: datetime | None = None

    def _read(self):
        if not os.path.exists(self.path):
            return
        with open(self.path, "rb") as f:
            f.seek(self._offset)
            data = f.read()
        end = data.rfind(b"\n") + 1  # a line still being written waits
        for text in data[:end].decode().splitlines():
            if text.strip():
                self.raw.append(text)
        self._offset += end

    def _apply_next(self):
        text = self.raw[self.i]
        self.w.apply(m.parse(text))
        line = json.loads(text)
        self.lines.append(line)
        self.history.saw(line)
        self.history.sample(self.w)
        self.i += 1

    def advance(self, seconds: float, speed: float):
        self._read()
        if self.live:
            while self.i < len(self.raw):
                self._apply_next()
            return
        if self.i >= len(self.raw):
            return
        if self.cursor is None:
            self.cursor = parse_t(json.loads(self.raw[0])["t"])
        self.cursor = self.cursor + (seconds * speed) * (parse_t("2000-01-01T00:01:00Z") - parse_t("2000-01-01T00:00:00Z"))
        while self.i < len(self.raw) and parse_t(json.loads(self.raw[self.i])["t"]) <= self.cursor:
            self._apply_next()
        if self.i < len(self.raw) and not self.lines:
            self._apply_next()

    def control(self, msg):
        if msg.get("action") == "live":
            self.live = True
        elif msg.get("action") == "reset":  # replay from the first line
            self.live = False
            self._restart()

    def info(self):
        return dict(live=self.live, lines_total=len(self.raw), path=self.path,
                    behind=len(self.raw) - self.i)


class Sim:
    kind = "sim"

    def __init__(self, params=None, seed=1):
        self.sim = simfactory.Sim(params, seed)
        self.history = History()
        self._seen = 0
        self.sim.env.process(self._sampler())

    def _sampler(self):
        while True:
            self._catch_up()
            self.history.sample(self.sim.w)
            yield self.sim.env.timeout(60)

    def _catch_up(self):
        for line in self.sim.lines[self._seen:]:
            self.history.saw(line)
        self._seen = len(self.sim.lines)

    @property
    def w(self):
        return self.sim.w

    @property
    def lines(self):
        return self.sim.lines

    def advance(self, seconds: float, speed: float):
        self.sim.run_until(self.sim.env.now + seconds * speed)
        self._catch_up()

    def control(self, msg):
        for k, v in (msg.get("params") or {}).items():
            if k in self.sim.p:
                self.sim.p[k] = float(v)
        if msg.get("params"):
            self.sim.poke()

    def info(self):
        return dict(seed=self.sim.seed, params=self.sim.p)


class Hub:
    def __init__(self, log: str):
        self.lock = threading.Lock()
        self.log = log
        self.real = Real(log)
        self.sim = Sim()
        self.source = self.real
        self.playing = True
        self.speed = {"real": 60.0, "sim": 60.0}  # simulated minutes per second
        self.seed = 1
        self.frame_n = 0

    def loop(self):
        last = time.monotonic()
        while True:
            time.sleep(0.1)
            now = time.monotonic()
            with self.lock:
                if self.playing or (self.source is self.real and self.real.live):
                    try:
                        self.source.advance(now - last, self.speed[self.source.kind])
                    except Exception as err:  # a refused line: show it, stop
                        self.playing = False
                        self.error = f"{type(err).__name__}: {err}"
            last = now

    def control(self, msg):
        with self.lock:
            if msg.get("source") in ("real", "sim"):
                self.source = self.real if msg["source"] == "real" else self.sim
            if "speed" in msg:
                self.speed[self.source.kind] = max(0.1, min(20000.0, float(msg["speed"])))
            if "seed" in msg:
                self.seed = int(msg["seed"])
            a = msg.get("action")
            if a == "play":
                self.playing = True
            elif a == "pause":
                self.playing = False
            elif a == "reset" and self.source is self.sim:
                self.sim = self.source = Sim(dict(self.sim.sim.p), self.seed)
            self.source.control(msg)
            return dict(ok=True)

    def usage(self):
        """state/usage.json beside the log, from factory usage, if there is one."""
        p = pathlib.Path(self.log).parent / "state" / "usage.json"
        try:
            return json.loads(p.read_text())
        except (OSError, ValueError):
            return None

    def beat(self):
        p = pathlib.Path(self.log).parent / "state" / "watchdog.json"
        try:
            return json.loads(p.read_text())
        except (OSError, ValueError):
            return None

    def frame(self):
        with self.lock:
            s = self.source
            out = snapshot(s.w, s.lines)
            if s is self.real:
                out["usage"] = out.get("usage") or self.usage()  # the log's readings first
                out["watchdog"] = self.beat()
            out.update(source=s.kind, playing=self.playing, speed=self.speed[s.kind],
                       info=s.info(), error=getattr(self, "error", None),
                       hours=len(s.history.rows))
            return out


HUB: Hub


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def send(self, body, ctype="application/json"):
        data = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(200)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        path = self.path.split("?")[0]
        if path == "/":
            self.send((HERE / "index.html").read_bytes(), "text/html; charset=utf-8")
        elif path == "/api/params":
            self.send([dict(key=k, group=g, label=l, min=a, max=b, step=s, default=d)
                       for k, g, l, a, b, s, d in simfactory.PARAMS])
        elif path == "/api/series":
            with HUB.lock:
                self.send(HUB.source.history.rows)
        elif path == "/api/stream":
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            try:
                while True:
                    self.wfile.write(b"data: " + json.dumps(HUB.frame()).encode() + b"\n\n")
                    self.wfile.flush()
                    time.sleep(0.25)
            except (BrokenPipeError, ConnectionResetError):
                pass
        else:
            self.send_error(404)

    def do_POST(self):
        if self.path != "/api/control":
            return self.send_error(404)
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        self.send(HUB.control(json.loads(body or b"{}")))


def main():
    global HUB
    ap = argparse.ArgumentParser()
    ap.add_argument("--log", default=os.environ.get("FACTORY_LOG", "log.jsonl"))
    ap.add_argument("--port", type=int, default=8377)
    ap.add_argument("--host", default="127.0.0.1")
    a = ap.parse_args()
    HUB = Hub(a.log)
    threading.Thread(target=HUB.loop, daemon=True).start()
    print(f"factory visualizer on http://{a.host}:{a.port} (log {a.log})", flush=True)
    ThreadingHTTPServer((a.host, a.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
