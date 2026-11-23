#!/usr/bin/env python3
"""Internet for the build server of the namespace rig (dev/netns.sh), which
has no route out: an HTTP CONNECT proxy that listens on a unix socket in the
host's network namespace, and a forwarder that listens on TCP inside the
server's namespace and passes connections to that socket. A unix socket is
a file, so it crosses network namespaces; no interface, route or firewall
rule is touched.

  rigproxy.py outside SOCKET          (run in the host namespace)
  rigproxy.py inside SOCKET PORT      (run in the server's namespace)

Actions that the policy lets use the network reach it with
HTTPS_PROXY=http://127.0.0.1:PORT.
"""
import os, socket, sys, threading

def pump(a, b):
    try:
        while True:
            d = a.recv(65536)
            if not d:
                break
            b.sendall(d)
    except OSError:
        pass
    for s in (a, b):
        try:
            s.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass

def join(a, b):
    threading.Thread(target=pump, args=(a, b), daemon=True).start()
    pump(b, a)
    a.close(); b.close()

def serve_connect(c):
    head = b""
    while b"\r\n\r\n" not in head:
        d = c.recv(4096)
        if not d:
            c.close(); return
        head += d
    first = head.split(b"\r\n", 1)[0].split()
    if len(first) < 2 or first[0] != b"CONNECT":
        c.sendall(b"HTTP/1.1 405 Method Not Allowed\r\n\r\n"); c.close(); return
    host, _, port = first[1].decode().rpartition(":")
    try:
        up = socket.create_connection((host, int(port)), timeout=30)
        up.settimeout(None)
    except OSError:
        c.sendall(b"HTTP/1.1 502 Bad Gateway\r\n\r\n"); c.close(); return
    c.sendall(b"HTTP/1.1 200 Connection established\r\n\r\n")
    rest = head.split(b"\r\n\r\n", 1)[1]
    if rest:
        up.sendall(rest)
    join(c, up)

def main():
    mode, path = sys.argv[1], sys.argv[2]
    if mode == "outside":
        if os.path.exists(path):
            os.unlink(path)
        ls = socket.socket(socket.AF_UNIX)
        ls.bind(path); ls.listen(64)
        while True:
            c, _ = ls.accept()
            threading.Thread(target=serve_connect, args=(c,), daemon=True).start()
    else:
        ls = socket.socket()
        ls.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        ls.bind(("127.0.0.1", int(sys.argv[3]))); ls.listen(64)
        while True:
            c, _ = ls.accept()
            u = socket.socket(socket.AF_UNIX)
            try:
                u.connect(path)
            except OSError:
                c.close(); continue
            threading.Thread(target=join, args=(c, u), daemon=True).start()

main()
