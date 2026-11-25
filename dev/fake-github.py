#!/usr/bin/env python3
import json, sys, urllib.parse
from http.server import BaseHTTPRequestHandler, HTTPServer

port, who_file = int(sys.argv[1]), sys.argv[2]


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def send(self, status, body, headers=None):
        data = json.dumps(body).encode() if not isinstance(body, bytes) else body
        self.send_response(status)
        for k, v in (headers or {"Content-Type": "application/json"}).items():
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        u = urllib.parse.urlparse(self.path)
        q = dict(urllib.parse.parse_qsl(u.query))
        if u.path == "/login/oauth/authorize":
            login = open(who_file).read().strip()
            back = q["redirect_uri"] + "?" + urllib.parse.urlencode({"code": "ghcode-" + login, "state": q["state"]})
            return self.send(302, b"", {"Location": back})
        if u.path == "/user":
            token = self.headers.get("Authorization", "").removeprefix("Bearer ")
            if not token.startswith("ghtoken-"):
                return self.send(401, {"message": "Bad credentials"})
            return self.send(200, {"login": token.removeprefix("ghtoken-"), "id": 1})
        self.send(404, {})

    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0))
        f = dict(urllib.parse.parse_qsl(self.rfile.read(n).decode()))
        if self.path == "/login/oauth/access_token":
            if f.get("client_secret") != "the-app-secret" or not f.get("code", "").startswith("ghcode-"):
                return self.send(200, {"error": "bad_verification_code", "error_description": "The code passed is incorrect or expired."})
            return self.send(200, {"access_token": "ghtoken-" + f["code"].removeprefix("ghcode-"), "token_type": "bearer"})
        self.send(404, {})


HTTPServer(("127.0.0.1", port), H).serve_forever()
