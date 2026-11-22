# Own iroh relay

gangli uses the **public n0 relays** by default **[decided 2026-11-20]**. The iroh docs advise against them in
production: they are shared, rate limited and without guarantees. gangli takes a relay list in its config
next to the binary (`relays`, `relays_n0_fallback`: `RelayMode::Custom`, ours first, n0 as fallback).

**Our relay [set up 2026-11-22]**: an Oracle Cloud Always Free VM (Ampere A1, 2 OCPU, 12 GB, Ubuntu), set up
by [`scripts/relay/setup.sh`](../scripts/relay/setup.sh); details in *What runs on the relay VM* below. Its
address, the SSH key and the knock file are the owner's, never in this repository.

## What runs on the relay VM

- **iroh-relay**, the release matching gangli's iroh (`v1.5.0`, checked against the release's SHA-256), as
  the `iroh-relay` system user with only `CAP_NET_BIND_SERVICE`: relaying on TCP 443, QUIC address discovery
  on UDP 7842; plain HTTP and metrics only on 127.0.0.1. URL: `https://<ip>/`.
- **Certificate for the bare IP**, no DNS name (so no DNS service to depend on): Let's Encrypt issues IP
  certificates only with its 6-day `shortlived` profile (generally available since 2026-01-15), through
  **certbot from the snap** (5.3+ for `--ip-address`; the Ubuntu package is too old), standalone on TCP 80,
  no email. The snap's timer renews it; hooks open TCP 80 only during the renewal and copy the new files
  into `/var/lib/iroh-relay`, which the relay reloads by itself (`cert_mode = "Reloading"`). A free
  `*.sslip.io` name was rejected: sslip.io is not on the Public Suffix List, so every sslip.io user in the
  world shares Let's Encrypt's 50-certificates-a-week limit.
- **Firewall that answers nothing**: the VM's iptables accept only established traffic, TCP 443 and UDP 7842,
  and DROP the rest (no ICMP host-prohibited, no ping): a scan sees the relay and nothing else. `rpcbind` is
  off. The cloud firewall (VCN security list) must let in TCP 443, UDP 7842, TCP 80 (renewals), TCP 22 and
  the knock port; the VM's own firewall is what hides them.
- **SSH only after a knock** ([`knockd.py`](../scripts/relay/knockd.py)): one UDP packet to a secret port,
  `timestamp | nonce | HMAC-SHA256(key, ...)`, valid for 30 s and never twice. Nothing listens on that port:
  the firewall drops it like any other and the daemon reads it from a packet socket, so it cannot be told
  apart from a closed port, and a wrong or replayed packet gets no answer at all. A valid knock opens TCP 22
  for 60 s to the sender's address only (iptables `recent` list); open sessions stay open. Clients:
  [`knock.ps1`](../scripts/relay/knock.ps1) (Windows, nothing to install) and
  [`knock.py`](../scripts/relay/knock.py); the knock file (`host port key-hex`) stays on the owner's
  computer, owner-only. Then ssh / PuTTY / WinSCP within 60 s, with the Pageant key (password login is off).
  With OpenSSH it can be automatic, in `~/.ssh/config`:

  ```
  Match host <ip> exec "powershell -NoProfile -File C:/path/to/knock.ps1 C:/Users/me/.ssh/relay.knock"
  ```
- **Not locking ourselves out**: `setup.sh` arms a 5-minute rollback (`fw-rollback` timer, restores the
  previous rules) before every firewall change; it is cancelled only after a *new* SSH session (with a knock)
  works. Last resort: the Oracle console's serial console or Run Command, independent of the network.
- **Keep-alive** (`oci-keepalive`): Oracle reclaims an Always Free instance when, over 7 days, **all** of CPU
  (95th percentile), network and memory (A1 shapes only) stay below 20%
  ([Always Free resources](https://docs.oracle.com/en-us/iaas/Content/FreeTier/resourceref.htm)). A relay
  is idle by those numbers, so a small service holds 25% of the RAM (touched hourly, near zero CPU) and,
  as a second guard, keeps one core busy 02:00–04:00 UTC in the idle scheduling class (8% of the time > the
  5% that sets the 95th percentile; it only gets CPU nobody else wants). The Oracle Cloud Agent, which reports
  those metrics, must stay installed. It is the first thing the OOM killer takes; the relay the last.

## Options considered (2026-11-20)

What a relay needs (iroh-relay, same crate n0 runs in production):

| Port | What | Needed for |
|---|---|---|
| TCP 443 (+80 for Let's Encrypt) | HTTPS + WebSocket relay protocol | relaying traffic when hole-punching fails |
| UDP 7842 | QUIC address discovery (QAD, off by default, needs TLS) | telling endpoints their public address, which helps hole-punching |

## 1. Small free VM (recommended)

An always-free VM with a public IP, e.g. **Oracle Cloud Always Free** (ARM, generous), Google Cloud
`e2-micro` free tier, or any 3–5 €/month VPS.

1. A DNS name pointing at the VM (`relay.example.org`); any free DNS / dynamic DNS works.
2. Download the `iroh-relay` release binary (or `cargo install iroh-relay`), config:

   ```toml
   enable_relay = true
   http_bind_addr = "[::]:80"
   enable_quic_addr_discovery = true

   [tls]
   cert_mode = "LetsEncrypt"
   hostname = "relay.example.org"
   contact = "someone@example.org"
   ```

3. Open TCP 80, 443 and UDP 7842 in the provider's firewall (on Oracle: the VCN security list *and* the
   VM's iptables).
4. Run it as a systemd service. This is the only machine of the whole system with open ports, and it sees
   only encrypted traffic.

## 2. Cloudflare Tunnel in front of a relay anywhere (free, no open port)

`iroh-relay` without QAD on any always-on machine (a home box, a NAS), published through a free Cloudflare
Tunnel (`cloudflared`): Cloudflare's edge terminates HTTPS and forwards the WebSocket relay protocol over the
tunnel, which only needs outbound connections. No public IP, no open port.

Limit: no UDP through the tunnel, so no QAD from this relay. Keep the n0 relays in the list for address
discovery, or accept a little less hole-punching success.

## 3. Cloudflare Workers / Durable Objects (not a real option yet)

Workers cannot send or receive UDP, so they cannot host a full relay. A community project runs an iroh
endpoint in a Durable Object speaking the relay protocol over inbound WebSockets only
([iroh-durable-objects](https://github.com/gotwalt/iroh-durable-objects)); outbound dialing is not
implemented. Others replaced iroh with their own Durable Object relay protocol, not iroh-compatible. Worth
watching, not worth building on today.

## Address lookup

Dialing by id also needs an address lookup service: n0's `dns.iroh.link` today. Self-hosting means
`iroh-dns-server` next to the relay (same VM, UDP/TCP 53 + HTTPS). The gangli directory itself already uses
the public Mainline DHT and pkarr relays, not n0.

## Sources

- iroh docs: [Relays](https://docs.iroh.computer/concepts/relays), [Add a relay](https://docs.iroh.computer/add-a-relay),
  [FAQ](https://docs.iroh.computer/about/faq), [Pricing](https://www.iroh.computer/pricing)
- [iroh-relay main.rs (config, QAD)](https://github.com/n0-computer/iroh/blob/main/iroh-relay/src/main.rs),
  [self-hosting notes](https://github.com/flexaccessdev/iroh-common-architecture/blob/main/self-hosting.md)
- [iroh 0.91: relays over WebSocket](https://www.iroh.computer/blog/iroh-0-91-0-the-last-relay-break)
- Durable Objects: [iroh-durable-objects](https://github.com/gotwalt/iroh-durable-objects),
  [Cloudflare WebSocket hibernation](https://developers.cloudflare.com/durable-objects/best-practices/rules-of-durable-objects/)
