#!/usr/bin/env python3
"""Seed a GreenMail/IMAP test server with N messages across nested folders.

Usage: seed.py <host> <port> <user> <password> [count]

Creates ~8 mailboxes (incl. nested hierarchy and a non-ASCII UTF-7 name)
and APPENDs `count` messages spread across them with mixed flags.
Deterministic: message i has subject 'TestMessage <i>' and a unique body.
"""

import imaplib
import sys
import time

FOLDERS = [
    "INBOX",
    "Work",
    "Work.Projects",
    "Work.Projects.2024",
    "Archive.Old",
    "Receipts",
    "Personal &- Family",  # modified UTF-7: 'Personal & Family'
    "Trash",
]

# Different flag sets cycled across messages: unseen, seen, flagged,
# answered+seen, draft, deleted
FLAGSETS = [
    None,
    "(\\Seen)",
    "(\\Flagged)",
    "(\\Seen \\Answered)",
    "(\\Draft)",
    "(\\Deleted)",
    "(\\Seen \\Flagged $Custom)",
]


def msg_bytes(i: int, folder: str) -> bytes:
    return (
        f"From: sender@example.com\r\n"
        f"To: test@localhost\r\n"
        f"Subject: TestMessage {i} in {folder}\r\n"
        f"Message-ID: <test-{i}@cryomail-e2e>\r\n"
        f"Date: Mon, 01 Jan 2024 12:00:00 +0000\r\n"
        f"X-Test-Index: {i}\r\n"
        f"\r\n"
        f"Body of test message {i}.\r\n"
        f"Some longer content to have a real message body. {i * 7919}\r\n"
    ).encode()


def connect(host: str, port: int, user: str, pw: str) -> imaplib.IMAP4:
    for attempt in range(30):
        try:
            m = imaplib.IMAP4(host, port)
            if m.login(user, pw)[0] == "OK":
                return m
        except OSError:
            pass
        time.sleep(1)
    raise SystemExit("cannot connect/login to seed server")


def main() -> None:
    host, port, user, pw = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
    count = int(sys.argv[5]) if len(sys.argv) > 5 else 1000

    m = connect(host, port, user, pw)
    for f in FOLDERS:
        typ, _ = m.create(f'"{f}"')
        print(f"create {f}: {typ}")

    for i in range(count):
        folder = FOLDERS[i % len(FOLDERS)]
        flags = FLAGSETS[i % len(FLAGSETS)]
        typ, _ = m.append(f'"{folder}"', flags, None, msg_bytes(i, folder))
        if typ != "OK":
            raise SystemExit(f"APPEND failed at message {i}")
    print(f"seeded {count} messages into {len(FOLDERS)} folders")
    m.logout()


if __name__ == "__main__":
    main()
