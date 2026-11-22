#!/usr/bin/env python3
# Single-packet knock for the relay VM's SSH (docs/RELAY.md). Nothing listens: the knock port is DROPped by the
# firewall like every closed port, and this daemon reads packets from a packet socket (it sees them before the
# firewall does). A valid knock lets the sender's address (and only it) open new connections to any port for
# 60 s (nftables set `inet host knock`, its elements expire by themselves); connections already open stay open.
#
# Knock: one UDP datagram of 56 bytes to the knock port:
#   ts (u64 BE, unix seconds) | nonce (16 random bytes) | HMAC-SHA256(key, "gangli-knock-v1" | ts | nonce)
# Valid when the MAC matches, ts is within 30 s of now and the nonce was not seen before (no replay).
# Anything else is ignored without a trace or an answer.
#
# usage: knockd.py <udp port> <key file: 64 hex chars, root-only>
import hashlib, hmac, socket, struct, subprocess, sys, time

PORT = int(sys.argv[1])
KEY = bytes.fromhex(open(sys.argv[2]).read().split()[-1])
LABEL = b"gangli-knock-v1"
WINDOW = 30
OPEN_FOR = "60s"
ETH_P_IP = 0x0800
PACKET_OUTGOING = 4

seen = {}  # nonce -> ts, forgotten after 2 windows


def valid(payload):
    if len(payload) != 56:
        return False
    ts = struct.unpack(">Q", payload[:8])[0]
    nonce, mac = payload[8:24], payload[24:]
    if abs(time.time() - ts) > WINDOW:
        return False
    want = hmac.new(KEY, LABEL + payload[:24], hashlib.sha256).digest()
    if not hmac.compare_digest(mac, want):
        return False
    now = time.time()
    for n, t in list(seen.items()):
        if now - t > 2 * WINDOW:
            del seen[n]
    if nonce in seen:
        return False
    seen[nonce] = now
    return True


def main():
    s = socket.socket(socket.AF_PACKET, socket.SOCK_DGRAM, socket.htons(ETH_P_IP))
    print(f"knockd: watching udp/{PORT}", flush=True)
    while True:
        pkt, addr = s.recvfrom(2048)
        if addr[2] == PACKET_OUTGOING or len(pkt) < 28 or pkt[0] >> 4 != 4 or pkt[9] != 17:
            continue
        ihl = (pkt[0] & 0x0F) * 4
        if len(pkt) < ihl + 8 or struct.unpack(">H", pkt[ihl + 2 : ihl + 4])[0] != PORT:
            continue
        if not valid(pkt[ihl + 8 :]):
            continue
        src = socket.inet_ntoa(pkt[12:16])
        # update: a second knock within the minute restarts the 60 s
        r = subprocess.run(
            ["/usr/sbin/nft", f"add element inet host knock {{ {src} timeout {OPEN_FOR} }}; "
                              f"delete element inet host knock {{ {src} }}; "
                              f"add element inet host knock {{ {src} timeout {OPEN_FOR} }}"],
            capture_output=True, text=True)
        if r.returncode == 0:
            print(f"knockd: open for {src} ({OPEN_FOR})", flush=True)
        else:
            print(f"knockd: cannot open for {src}: {r.stderr.strip()}", flush=True)


if __name__ == "__main__":
    main()
