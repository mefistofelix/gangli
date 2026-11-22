#!/bin/bash
# Sets up a relay VM for gangli (docs/RELAY.md), run ON the VM (Ubuntu, sudo), from the project root here:
#
#   ssh ubuntu@VM 'cat > /tmp/knockd.py' < scripts/relay/knockd.py
#   awk '{print $3}' KNOCKFILE | ssh ubuntu@VM 'sudo install -m 600 /dev/stdin /etc/knockd.key'
#   awk '{print "KNOCK_PORT=" $2}' KNOCKFILE | ssh ubuntu@VM 'sudo install -m 600 /dev/stdin /etc/knockd.env'
#   (the cloud firewall lets everything in: on Oracle one ingress rule, all protocols from 0.0.0.0/0)
#   ssh ubuntu@VM 'bash -s -- IP open' < scripts/relay/setup.sh      # SSH still reachable without a knock
#   knock.ps1 KNOCKFILE; ssh ubuntu@VM 'sudo fw-confirm'               # a NEW session got in: keep the rules
#   ssh ubuntu@VM 'bash -s -- IP knock' < scripts/relay/setup.sh     # everything but the relay after a knock
#   knock.ps1 KNOCKFILE; ssh ubuntu@VM 'sudo fw-confirm'
#
# KNOCKFILE is one line "host port key-hex" kept only on your computer (knock.ps1 / knock.py read it).
# What it installs: iroh-relay (release matching gangli's iroh) as a service on TCP 443 + UDP 7842 at
# https://<ip with dashes>.sslip.io/ with its own Let's Encrypt certificate (built-in ACME, TLS-ALPN-01 on
# 443: nothing else), the knock daemon, a firewall that drops everything else without answering, and a
# keep-alive against Oracle's idle reclaim.
# Safe to run again. Every firewall change arms a 5-minute rollback first; the last step cancels it.
set -e
IP=${1:?usage: setup.sh IP open|knock}
MODE=${2:-open}
HOST=$(echo "$IP" | tr . -).sslip.io
VER=v1.5.0
SHA=99a14d2ff541711f1b175a15e0c1571a45f46c171d655cda29849d91d90a5c9c  # iroh-relay-$VER-aarch64-unknown-linux-musl.tar.gz
[ "$(uname -m)" = aarch64 ] || { echo "SHA is for aarch64: set VER/SHA for $(uname -m)"; exit 2; }

# ---- iroh-relay ----
if [ "$(/usr/local/bin/iroh-relay --version 2>/dev/null)" != "iroh-relay ${VER#v}" ]; then
  d=$(mktemp -d)
  curl -fsSLo "$d/r.tgz" "https://github.com/n0-computer/iroh/releases/download/$VER/iroh-relay-$VER-aarch64-unknown-linux-musl.tar.gz"
  echo "$SHA  $d/r.tgz" | sha256sum -c -
  tar xzf "$d/r.tgz" -C "$d"
  sudo install -m 0755 "$(find "$d" -type f -name iroh-relay | head -1)" /usr/local/bin/iroh-relay
  rm -rf "$d"
fi
id iroh-relay > /dev/null 2>&1 || sudo useradd --system --no-create-home --shell /usr/sbin/nologin iroh-relay
sudo install -d -o iroh-relay -g iroh-relay -m 0750 /var/lib/iroh-relay
sudo tee /etc/iroh-relay.toml > /dev/null <<EOF
# iroh relay for gangli (docs/RELAY.md): https://$HOST/ ($HOST is $IP, sslip.io answers it). The relay
# gets and renews its own Let's Encrypt certificate (TLS-ALPN-01 on 443), kept in /var/lib/iroh-relay.
enable_relay = true
# plain HTTP (captive portal check) stays local: one open port less
http_bind_addr = "127.0.0.1:3340"
enable_quic_addr_discovery = true
enable_metrics = true
metrics_bind_addr = "127.0.0.1:9090"

[tls]
https_bind_addr = "[::]:443"
quic_bind_addr = "[::]:7842"
cert_mode = "LetsEncrypt"
hostname = "$HOST"
# required by iroh-relay; Let's Encrypt no longer sends mail, so not a real mailbox
contact = "relay@$HOST"
cert_dir = "/var/lib/iroh-relay"
EOF
sudo tee /etc/systemd/system/iroh-relay.service > /dev/null <<'EOF'
[Unit]
Description=iroh relay (gangli)
After=network-online.target
Wants=network-online.target

[Service]
User=iroh-relay
Group=iroh-relay
ExecStart=/usr/local/bin/iroh-relay --config-path /etc/iroh-relay.toml
Restart=always
RestartSec=5
Environment=RUST_LOG=info
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ReadWritePaths=/var/lib/iroh-relay
LimitNOFILE=65536
OOMScoreAdjust=-500

[Install]
WantedBy=multi-user.target
EOF

# ---- keep-alive: Oracle reclaims an Always Free instance when, over 7 days, CPU (95th percentile), network
# and memory (A1 shapes) all stay below 20% ----
sudo tee /usr/local/bin/oci-keepalive > /dev/null <<'EOF'
#!/usr/bin/env python3
# Keeps this Always Free instance from being judged idle (docs/RELAY.md):
#  - memory: hold 25% of RAM, touched once an hour so it stays resident (no CPU cost to speak of);
#  - CPU, a second guard: one core busy 02:00-04:00 UTC every day (8% of the time, more than the 5% that sets
#    the 95th percentile), in the idle scheduling class: it only takes CPU nobody else wants.
import mmap, os, time
total = os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES")
size = int(total * float(os.environ.get("KEEPALIVE_MEMORY", "0.25")))
buf = mmap.mmap(-1, size, flags=mmap.MAP_PRIVATE | mmap.MAP_ANONYMOUS)
def touch():
    for i in range(0, size, mmap.PAGESIZE):
        buf[i] = (buf[i] + 1) & 0xFF
touch()
print(f"holding {size >> 20} MiB of {total >> 20} MiB", flush=True)
last = time.monotonic()
while True:
    if 2 <= time.gmtime().tm_hour < 4:
        end, x = time.monotonic() + 60, 0
        while time.monotonic() < end:
            x = (x * 1103515245 + 12345) & 0xFFFFFFFF
    else:
        time.sleep(60)
    if time.monotonic() - last > 3600:
        touch(); last = time.monotonic()
EOF
sudo chmod 0755 /usr/local/bin/oci-keepalive
sudo tee /etc/systemd/system/oci-keepalive.service > /dev/null <<'EOF'
[Unit]
Description=Keep this Oracle Always Free instance from being reclaimed as idle

[Service]
ExecStart=/usr/local/bin/oci-keepalive
Restart=always
RestartSec=60
DynamicUser=true
Nice=19
CPUSchedulingPolicy=idle
IOSchedulingClass=idle
CPUQuota=100%
OOMScoreAdjust=1000
ProtectSystem=strict
ProtectHome=true
NoNewPrivileges=true

[Install]
WantedBy=multi-user.target
EOF

# ---- knock daemon (scripts/relay/knockd.py; key and port copied beforehand) ----
[ -f /tmp/knockd.py ] && sudo install -m 0755 /tmp/knockd.py /usr/local/bin/knockd.py && rm -f /tmp/knockd.py
[ -x /usr/local/bin/knockd.py ] && [ -f /etc/knockd.key ] && [ -f /etc/knockd.env ] || { echo "copy knockd.py, /etc/knockd.key and /etc/knockd.env first"; exit 2; }
sudo tee /etc/systemd/system/knockd.service > /dev/null <<'EOF'
[Unit]
Description=Single-packet knock for SSH (no listening port)
After=network-online.target

[Service]
EnvironmentFile=/etc/knockd.env
ExecStart=/usr/bin/python3 -I /usr/local/bin/knockd.py ${KNOCK_PORT} /etc/knockd.key
Restart=always
RestartSec=3
CapabilityBoundingSet=CAP_NET_RAW CAP_NET_ADMIN CAP_DAC_OVERRIDE
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true

[Install]
WantedBy=multi-user.target
EOF

# ---- nothing else listening ----
sudo systemctl disable --now rpcbind.socket rpcbind.service > /dev/null 2>&1 || true

# ---- firewall (nftables; the cloud firewall lets everything in, this one decides): answer nothing but the
# relay; SSH only after a knock (MODE=knock) ----
SSH_OPEN=""
[ "$MODE" = knock ] || SSH_OPEN="tcp dport 22 accept  # MODE=open: SSH without a knock"
sudo tee /etc/nftables.conf.new > /dev/null <<EOF
#!/usr/sbin/nft -f
# gangli relay VM (scripts/relay/setup.sh, docs/RELAY.md). Mode: $MODE.
flush ruleset

table inet host {
    # addresses that knocked (scripts/relay/knockd.py adds them with a 60 s timeout)
    set knock {
        type ipv4_addr
        flags timeout
    }

    # Nothing new gets in but these two ports (TCP and UDP only: no ping, no ICMP, no IPv6), and everything
    # from an address that knocked in the last 60 s. "related" lets in only the ICMP errors about
    # connections already open (e.g. "fragmentation needed"), which path MTU discovery needs.
    chain input {
        type filter hook input priority filter; policy drop;
        ct state established,related accept
        ct state invalid drop
        iif lo accept
        ip saddr @knock accept                    # knocked: every port, every protocol, for 60 s
        tcp dport 443 accept                      # iroh relay (HTTPS / WebSocket, TLS-ALPN-01)
        udp dport 7842 accept                     # iroh QUIC address discovery
        $SSH_OPEN
        udp sport 67 udp dport 68 accept          # DHCP answers from Oracle
    }

    chain forward {
        type filter hook forward priority filter; policy drop;
    }

    chain output {
        type filter hook output priority filter; policy accept;
        ip daddr 169.254.0.0/16 jump instance_services
    }

    # Oracle's rules for its instance services (iSCSI boot volume, metadata, DNS, NTP), kept as shipped in the
    # image: see "Oracle-Provided Images" in the OCI docs before changing them
    chain instance_services {
        ip daddr 169.254.0.2 tcp dport 3260 meta skuid 0 accept
        ip daddr { 169.254.2.0/24, 169.254.4.0/24, 169.254.5.0/24 } tcp dport 3260 meta skuid 0 accept
        ip daddr 169.254.0.2 tcp dport 80 accept
        ip daddr 169.254.169.254 udp dport 53 accept
        ip daddr 169.254.169.254 tcp dport 53 accept
        ip daddr 169.254.0.3 tcp dport 80 meta skuid 0 accept
        ip daddr 169.254.0.4 tcp dport 80 accept
        ip daddr 169.254.169.254 tcp dport 80 accept
        ip daddr 169.254.169.254 udp dport { 67, 69, 123 } accept
        meta l4proto tcp reject with tcp reset
        meta l4proto udp reject with icmpx type port-unreachable
    }
}
EOF
sudo nft -c -f /etc/nftables.conf.new
# confirming: the new rules for good (nftables.service), iptables-persistent gone
sudo tee /usr/local/sbin/fw-confirm > /dev/null <<'EOF'
#!/bin/sh
# run from a NEW ssh session once it works: keeps the firewall just applied by setup.sh
set -e
systemctl stop fw-rollback.timer 2>/dev/null || true
mv /etc/nftables.conf.new /etc/nftables.conf
systemctl enable nftables > /dev/null 2>&1
if dpkg -s iptables-persistent > /dev/null 2>&1; then
  systemctl disable --now netfilter-persistent > /dev/null 2>&1 || true
  DEBIAN_FRONTEND=noninteractive apt-get purge -y -qq iptables-persistent netfilter-persistent > /dev/null
  rm -rf /etc/iptables
fi
echo "firewall kept: $(sed -n 2p /etc/nftables.conf)"
EOF
sudo chmod 0755 /usr/local/sbin/fw-confirm
# safety net: unless confirmed, everything is let in again in 5 minutes (only sshd and the relay listen)
sudo systemctl stop fw-rollback.timer fw-rollback.service 2>/dev/null || true
sudo systemd-run --quiet --unit=fw-rollback --on-active=5min /usr/sbin/nft flush ruleset
sudo nft -f /etc/nftables.conf.new

sudo systemctl daemon-reload
sudo systemctl enable oci-keepalive knockd > /dev/null 2>&1
sudo systemctl restart oci-keepalive knockd
# the relay asks Let's Encrypt for its certificate by itself (and retries until TCP 443 is reachable)
sudo systemctl enable iroh-relay > /dev/null 2>&1
if ! systemctl is-active --quiet iroh-relay || [ /etc/iroh-relay.toml -nt /run/iroh-relay.applied ]; then
  sudo systemctl restart iroh-relay && sudo touch /run/iroh-relay.applied
fi

echo "firewall ($MODE) applied: everything is let in again in 5 minutes unless, from a NEW ssh session"
echo "(after a knock, in knock mode), you run: sudo fw-confirm"
