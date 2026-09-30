#!/usr/bin/env python3
"""Verify that a restore target matches the backup source.

Usage: verify.py <hostA> <portA> <hostB> <portB> <user> <password> [dst_prefix]

For every selectable mailbox on A, finds the corresponding mailbox on B
(optionally under a prefix using the destination's hierarchy delimiter) and
compares the multiset of (flags-sans-Recent, sha256(body)) per folder.
Exits nonzero on any mismatch.
"""

import hashlib
import imaplib
import re
import sys
import time
from collections import Counter


def connect(host: str, port: int, user: str, pw: str):
    for _ in range(30):
        try:
            m = imaplib.IMAP4(host, port)
            if m.login(user, pw)[0] == "OK":
                return m
        except OSError:
            time.sleep(1)
    raise SystemExit(f"cannot connect to {host}:{port}")


def mailboxes(m) -> list[tuple[str, str]]:
    """[(name, delimiter)] of selectable mailboxes."""
    typ, data = m.list()
    assert typ == "OK"
    out = []
    for line in data:
        if not line:
            continue
        s = line.decode()
        attrs, _, rest = s.partition(")")
        if "\\Noselect" in attrs:
            continue
        delim = rest.strip().split(" ")[0].strip('"')
        name = rest.strip().split(" ", 1)[1].strip('"')
        out.append((name, delim if delim != "NIL" else "/"))
    return out


def folder_digest(m, name: str) -> Counter:
    """Multiset of (flags-without-Recent, sha256(body)) per message.
    Counter, not sorted list — frozensets have no total ordering."""
    typ, sel = m.select(f'"{name}"', readonly=True)
    assert typ == "OK", f"select {name}"
    if int(sel[0]) == 0:
        return Counter()
    _, fdata = m.fetch("1:*", "(FLAGS)")
    flags = []
    for d in fdata:
        if isinstance(d, bytes):
            mm = re.search(rb"FLAGS \(([^)]*)\)", d)
            flags.append(
                frozenset(mm.group(1).split()) - {b"\\Recent"} if mm else frozenset()
            )
    _, bdata = m.fetch("1:*", "(BODY.PEEK[])")
    bodies = [d[1] for d in bdata if isinstance(d, tuple)]
    assert len(flags) == len(bodies), f"{name}: {len(flags)} flags vs {len(bodies)} bodies"
    return Counter((f, hashlib.sha256(b).hexdigest()) for f, b in zip(flags, bodies))


def main() -> None:
    ha, pa, hb, pb, user, pw = (
        sys.argv[1],
        int(sys.argv[2]),
        sys.argv[3],
        int(sys.argv[4]),
        sys.argv[5],
        sys.argv[6],
    )
    prefix = sys.argv[7] if len(sys.argv) > 7 else ""

    a = connect(ha, pa, user, pw)
    b = connect(hb, pb, user, pw)

    dst_boxes = dict(mailboxes(b))
    delim_b = next(iter(dst_boxes.values()), "/")

    failures = 0
    total_msgs = 0
    for name, _ in mailboxes(a):
        target = f"{prefix}{delim_b}{name}" if prefix else name
        src = folder_digest(a, name)
        if target not in dst_boxes:
            print(f"MISSING mailbox on target: {target!r} (expected {sum(src.values())} msgs)")
            failures += sum(src.values())
            continue
        dst = folder_digest(b, target)
        if src == dst:
            print(f"OK   {name}: {sum(src.values())} messages identical")
        else:
            print(f"FAIL {name} -> {target!r}: {sum(src.values())} src vs {sum(dst.values())} dst")
            sm = {h for _, h in src}
            dm = {h for _, h in dst}
            print(f"     missing bodies on dst: {len(sm - dm)}, extra: {len(dm - sm)}")
            for (f, h), cnt in list((src - dst).items())[:5]:
                print(f"     src-only: {sorted(f)} {h[:12]} x{cnt}")
            failures += sum((src - dst).values()) + sum((dst - src).values())
        total_msgs += sum(src.values())
    a.logout()
    b.logout()

    if failures:
        print(f"VERIFY FAILED: {failures} mismatched messages")
        sys.exit(1)
    print(f"VERIFY OK: {total_msgs} messages across all mailboxes identical")


if __name__ == "__main__":
    main()
