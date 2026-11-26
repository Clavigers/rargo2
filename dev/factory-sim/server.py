#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = ["simpy"]
# ///
"""Serves the factory simulation. The browser page only draws what this serves.

  uv run server.py                 run the server on http://127.0.0.1:8377
  uv run server.py --compare 60    run every preset for 60 days and print the numbers

GET  /             the page
GET  /api/graph    nodes, edges, parameters, presets
GET  /api/stream   server-sent events: clock, node states, metrics, item moves
GET  /api/series   hourly history for the charts
POST /api/control  {"action": "play"|"pause"|"reset", "speed": n, "seed": n, "params": {...}}
"""
import json
import pathlib
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import sim

HERE = pathlib.Path(__file__).parent


class Hub:
    def __init__(self):
        self.lock = threading.Lock()
        self.seed = 1
        self.sim = sim.Sim(seed=self.seed)
        self.playing = True
        self.speed = 30.0  # simulated minutes per real second

    def loop(self):
        last = time.monotonic()
        while True:
            time.sleep(0.04)
            now = time.monotonic()
            with self.lock:
                if self.playing:
                    self.sim.run_until(self.sim.t + (now - last) * self.speed)
            last = now

    def control(self, msg):
        with self.lock:
            s = self.sim
            for k, v in (msg.get("params") or {}).items():
                if k in s.p:
                    s.p[k] = float(v)
            s.poke()
            if "speed" in msg:
                self.speed = max(0.1, min(20000.0, float(msg["speed"])))
            if "seed" in msg:
                self.seed = int(msg["seed"])
            action = msg.get("action")
            if action == "play":
                self.playing = True
            elif action == "pause":
                self.playing = False
            elif action == "reset":
                self.sim = sim.Sim(dict(s.p), self.seed, s.move_n)
            return dict(ok=True)

    def frame(self, last):
        with self.lock:
            s = self.sim
            out = s.snapshot()
            out.update(playing=self.playing, speed=self.speed, seed=self.seed, params=s.p,
                       moves=[m[1:] for m in s.moves if m[0] > last][-80:])
            return out, s.move_n


HUB = Hub()


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
        elif path == "/api/graph":
            self.send(sim.graph())
        elif path == "/api/series":
            with HUB.lock:
                self.send(HUB.sim.series)
        elif path == "/api/stream":
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            with HUB.lock:
                last = HUB.sim.move_n
            try:
                while True:
                    out, last = HUB.frame(last)
                    self.wfile.write(b"data: " + json.dumps(out).encode() + b"\n\n")
                    self.wfile.flush()
                    time.sleep(0.1)
            except (BrokenPipeError, ConnectionResetError):
                pass
        else:
            self.send_error(404)

    def do_POST(self):
        if self.path != "/api/control":
            return self.send_error(404)
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        self.send(HUB.control(json.loads(body or b"{}")))


def compare(days):
    names = list(sim.PRESETS)
    runs = []
    for name in names:
        rows = []
        for seed in range(1, 6):
            s = sim.Sim({**sim.defaults(), **sim.PRESETS[name]}, seed)
            s.run_until(days * sim.DAY)
            rows.append([v for _, v, _ in s.metrics()])
        labels = [l for l, _, _ in s.metrics()]
        runs.append([sum(col) / len(col) for col in zip(*rows)])
    print(f"{days} simulated days, mean of 5 seeds")
    print(f"{'':36}" + "".join(f"{n:>12}" for n in names))
    for i, label in enumerate(labels):
        print(f"{label:36}" + "".join(f"{r[i]:12.1f}" for r in runs))


if __name__ == "__main__":
    if len(sys.argv) > 2 and sys.argv[1] == "--compare":
        compare(float(sys.argv[2]))
    else:
        port = int(sys.argv[1]) if len(sys.argv) > 1 else 8377
        threading.Thread(target=HUB.loop, daemon=True).start()
        print(f"factory sim on http://127.0.0.1:{port}")
        ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
