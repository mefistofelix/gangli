# Own iroh relay — options for later

gangli uses the **public n0 relays** for now **[decided 2026-11-20]**. The iroh docs advise against them in
production: they are shared, rate limited and without guarantees. This page lists ways to run our own relay
for free or nearly, researched 2026-11-20, **not tried yet**. Whatever is chosen, gangli will take a relay
list in its config next to the binary (`RelayMode::Custom`), keeping the n0 relays as fallback.

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
