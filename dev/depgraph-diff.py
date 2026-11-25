"""Compares two dep-graph.bin files record by record (#114).

  python3 -I dev/depgraph-diff.py OLD NEW

Prints how many of NEW's records are byte-identical to a record of OLD,
how many have the same node (kind and key) but differ, how many nodes are
new, and how far NEW's record order agrees with OLD's.
"""
import struct
import sys


def leb(d, p):
    v = s = 0
    while True:
        b = d[p]
        p += 1
        v |= (b & 0x7F) << s
        s += 7
        if b < 0x80:
            return v, p


def parse(path):
    d = open(path, "rb").read()
    # The file ends with the three counts and the encoder's end marker.
    end = b"rust-end-file"
    if d.endswith(end):
        d = d[: -len(end)]
    node_max, node_count, edge_count = struct.unpack("<QQQ", d[-24:])
    # After the file header (magic, format version, the rustc version) come
    # a few leb128 values of the session; the records start somewhere after.
    head_end = 7 + d[6]
    for start, kind_bits in ((s, k) for s in range(head_end, head_end + 80) for k in range(5, 13)):
        recs, p, ok = [], start, True
        try:
            for _ in range(node_count):
                head = struct.unpack_from("<H", d, p)[0]
                index = struct.unpack_from("<I", d, p + 2)[0]
                kind = head & ((1 << kind_bits) - 1)
                bpi = ((head >> kind_bits) & 3) + 1
                ln = head >> (kind_bits + 2)
                q = p + 38
                if ln == 0:
                    n, q = leb(d, q)
                else:
                    n = ln - 1
                q += n * bpi
                if index >= node_max or q > len(d):
                    ok = False
                    break
                recs.append((kind, d[p + 6:p + 22], index, d[p:q]))
                p = q
        except (struct.error, IndexError):
            ok = False
        if ok and len(recs) == node_count and len(d) - p < 4 * 512 + 64:
            return recs, node_max
    sys.exit(f"{path}: cannot parse")


def main():
    old, old_max = parse(sys.argv[1])
    new, new_max = parse(sys.argv[2])
    by_bytes = {r[3] for r in old}
    by_node = {(r[0], r[1]): i for i, r in enumerate(old)}
    same = changed = fresh = 0
    same_bytes = changed_bytes = fresh_bytes = 0
    order = []
    for r in new:
        key = (r[0], r[1])
        if r[3] in by_bytes:
            same += 1
            same_bytes += len(r[3])
        elif key in by_node:
            changed += 1
            changed_bytes += len(r[3])
        else:
            fresh += 1
            fresh_bytes += len(r[3])
        if key in by_node:
            order.append(by_node[key])
    # Records of NEW that follow the record that followed them in OLD.
    follows = sum(1 for a, b in zip(order, order[1:]) if b == a + 1)
    print(f"index space {old_max} -> {new_max}; records {len(old)} -> {len(new)}")
    print(f"identical {same} ({same_bytes} B), same node changed {changed} ({changed_bytes} B), "
          f"new {fresh} ({fresh_bytes} B)")
    print(f"order: {follows} of {len(order) - 1} consecutive pairs as in OLD")


main()
