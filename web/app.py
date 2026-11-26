import hashlib
import os

from flask import Flask, redirect, request, send_from_directory
from werkzeug.security import safe_join

SITE_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "site")

app = Flask(__name__)

_asset_hashes = {}


@app.template_global()
def asset_v(rel_path):
    """Content hash of a site file, for cache-busting query strings. Paired
    assets (e.g. wasm-bindgen's .js glue + .wasm) must never mix builds;
    hashing the content means the URL changes exactly when the file does.
    Cached for the process lifetime — deploys restart the process."""
    if rel_path not in _asset_hashes:
        with open(os.path.join(SITE_DIR, rel_path), "rb") as f:
            _asset_hashes[rel_path] = hashlib.sha256(f.read()).hexdigest()[:12]
    return _asset_hashes[rel_path]


@app.after_request
def cache_policy(response):
    # Cloudflare edge-caches css/js by extension unless told otherwise, which
    # has bitten us before; fonts and images are content-stable so cache long.
    path = request.path
    if path.endswith((".woff2", ".ttf", ".png", ".jpg", ".ico")):
        response.headers["Cache-Control"] = "public, max-age=2592000"
    else:
        response.headers["Cache-Control"] = "no-cache"
    return response


RELEASED = "%RARGO_VERSION%"


def released_version():
    """The version the release put on the /data volume, the one install.sh
    and `cargo install rargo` give; "unreleased" before the first release."""
    try:
        with open(os.path.join(DL_DIR, "VERSION")) as f:
            return f.read().strip() or "unreleased"
    except OSError:
        return "unreleased"


def page(rel_path):
    """A built page with the released version filled in, so no page names a
    version other than the one people install."""
    path = safe_join(SITE_DIR, rel_path)
    if path is None or not os.path.isfile(path):
        return "not found\n", 404, {"Content-Type": "text/plain"}
    with open(path, encoding="utf-8") as f:
        html = f.read()
    return html.replace(RELEASED, released_version()), 200, {"Content-Type": "text/html; charset=utf-8"}


@app.get("/")
def home():
    return page("index.html")


# The Book is the main page; keep the nav link but send it home.
@app.get("/book")
def book():
    return redirect("/")


@app.get("/reference")
def reference():
    return page("reference/index.html")


@app.get("/bibliography")
def bibliography():
    return send_from_directory(SITE_DIR, "bibliography/index.html")


# Phase 2 (accounts + customer dashboard) slots in here: copy the users/
# sessions tables, current_user(), and paid-gating from
# murraypedia/consulting/app.py, with SQLite on the /data volume that
# deploy.sh already mounts.

# --- What the rargo client and the installers ask of this name ---------
#
# `rargo @rargo.rs` and `rargo auth` talk HTTPS to rargo.rs, which the tunnel
# brings here; the build server speaks its own protocol on a port the tunnel
# does not carry. The server answers these few paths over plain HTTP on that
# port, and they are passed on to it as they are.
RARGO_UPSTREAM = os.environ.get("RARGO_UPSTREAM", "http://build.rargo.rs:7459")
# Released files (client binary, server archive, installers), put on the
# /data volume by the release process: nothing here names a version that is
# not released.
DL_DIR = os.environ.get("RARGO_DL_DIR", "/data/dl")


def _to_build_server():
    import urllib.error
    import urllib.request

    url = RARGO_UPSTREAM + request.full_path.rstrip("?")
    body = request.get_data() if request.method == "POST" else None
    headers = {"X-Forwarded-Host": request.host}
    if request.content_type:
        headers["Content-Type"] = request.content_type
    req = urllib.request.Request(url, data=body, method=request.method, headers=headers)

    # A redirect (signing in with GitHub) is for the browser, not for us.
    class NoRedirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, *a, **k):
            return None

    try:
        with urllib.request.build_opener(NoRedirect).open(req, timeout=20) as r:
            return r.read(), r.status, {"Content-Type": r.headers.get("Content-Type", "text/plain")}
    except urllib.error.HTTPError as e:
        out = {"Content-Type": e.headers.get("Content-Type", "text/plain")}
        if e.headers.get("Location"):
            out["Location"] = e.headers["Location"]
        return e.read(), e.code, out
    except Exception:
        return "the build server is not answering right now\n", 502, {"Content-Type": "text/plain"}


@app.get("/.well-known/rargo.json")
def rargo_lookup():
    return _to_build_server()


@app.get("/auth")
def rargo_auth_page():
    return _to_build_server()


@app.get("/auth/github")
def rargo_auth_github():
    return _to_build_server()


@app.get("/auth/github/callback")
def rargo_auth_github_back():
    return _to_build_server()


@app.post("/auth/<step>")
def rargo_auth_step(step):
    if step not in ("start", "poll", "approve", "revoke", "whoami"):
        return "not found\n", 404
    return _to_build_server()


@app.get("/install.sh")
def install_client():
    return send_from_directory(os.path.join(DL_DIR, "latest"), "install.sh", mimetype="text/plain")


@app.get("/install-server.sh")
def install_server():
    return send_from_directory(os.path.join(DL_DIR, "latest"), "install-server.sh", mimetype="text/plain")


@app.get("/dl/<path:name>")
def download(name):
    # As bytes, whatever the name ends in: a guessed `Content-Encoding: gzip`
    # makes clients unpack an archive on the way, and its checksum no
    # longer matches.
    kind = "text/plain" if name.endswith((".sh", "SHA256SUMS", "VERSION")) else "application/octet-stream"
    return send_from_directory(DL_DIR, name, mimetype=kind)


@app.get("/protocol")
def protocol():
    return send_from_directory(os.path.join(DL_DIR, "latest"), "protocol.html")


# Static assets last: concrete routes above win over the path converter.
@app.get("/<path:name>")
def asset(name):
    if name.endswith(".html"):
        return page(name)
    return send_from_directory(SITE_DIR, name)
