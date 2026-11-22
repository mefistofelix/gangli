#!/usr/bin/env python3
# Knock on the relay VM (scripts/relay/knockd.py, docs/RELAY.md) from Linux / macOS: SSH opens to this
# address for 60 s. The knock file is one line "host port key-hex", readable only by you.
# usage: knock.py <knock file>        then ssh within 60 s
import hashlib, hmac, os, socket, struct, sys, time

host, port, key = open(sys.argv[1]).read().split()
head = struct.pack(">Q", int(time.time())) + os.urandom(16)
mac = hmac.new(bytes.fromhex(key), b"gangli-knock-v1" + head, hashlib.sha256).digest()
socket.socket(socket.AF_INET, socket.SOCK_DGRAM).sendto(head + mac, (host, int(port)))
print(f"knocked on {host}")
