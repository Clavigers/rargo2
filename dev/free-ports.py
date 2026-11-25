import random
import socket
import sys

n = int(sys.argv[1])
low = int(open("/proc/sys/net/ipv4/ip_local_port_range").read().split()[0])


def free(port):
    held = []
    try:
        for kind in (socket.SOCK_STREAM, socket.SOCK_DGRAM):
            s = socket.socket(socket.AF_INET, kind)
            held.append(s)
            s.bind(("0.0.0.0", port))
        return True
    except OSError:
        return False
    finally:
        for s in held:
            s.close()


for _ in range(500):
    base = random.randrange(20000, low - n)
    if all(free(p) for p in range(base, base + n)):
        print(base)
        sys.exit(0)
sys.exit("no free ports below the ephemeral range")
