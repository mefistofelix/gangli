# gangli

Remote management for your own machines and those of family and friends. One binary for Windows, Linux and
macOS, three roles:

- **agent** — system service on the managed machine; joins with a join code, no open ports;
- **master** (control center) — same service, holds a network's key, admits agents;
- **client** — your PC: node list, ssh / terminal / PuTTY, forwards and SOCKS into their LAN, local API for
  scripts and LLM agents.

Peer-to-peer over [iroh](https://github.com/n0-computer/iroh) (hole-punching, relay fallback, multipath);
sessions survive disconnects and slow mobile links.

Status: design phase. See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) and [AGENTS.md](AGENTS.md).
