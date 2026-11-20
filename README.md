# gangli

Remote management for your own machines and those of family and friends: one small file for Windows, Linux
and macOS, peer to peer over [iroh](https://github.com/n0-computer/iroh) (hole punching, relay fallback), no
open ports, no server. Sessions survive disconnects and slow mobile links.

- **node**: a machine to manage. Run gangli with the network's public code (or a copy that carries it), by
  hand or installed as a system service.
- **console**: where you manage them, with the network's private code. The window (with a tray icon), or the
  command line, or a local API for scripts and AI agents.

A network is a key: whoever has its private code owns it; its public code only lets a machine join.

## What it does

- Live list of the machines: online or not, system, LAN address, whether the link is direct or relayed.
- Terminals: inside the window (with predictive echo on slow links), or any SSH client through the local
  gateway (`ssh -p 2222 user+machine@127.0.0.1`), PuTTY / OpenSSH / WinSCP from a menu; shells as any user,
  persistent terminals that keep running when you leave; SFTP.
- Tunnels to addresses on their network, saved and back by themselves; `ssh -D` for everything else.
- For scripts and AI agents: every command with `--json` and stable exit codes, and a local API whose admin
  token can do everything the window does: list and identify the machines, run commands, connect over SSH
  with any client, make tunnels ([docs/API.md](docs/API.md)).
- SSH keys instead of passwords: with your Pageant key in the gateway, PuTTY and WinSCP log in by themselves.
- Updates sent from the console to the machines.
- Portable: everything it keeps is in `gangli.node` and `gangli.console` next to the binary.

## Get it

Binaries are on the [releases](https://github.com/mefistofelix/gangli/releases) page:
`gangli-windows-x86_64.exe`, `gangli-linux-x86_64`, `gangli-linux-aarch64` (static, no dependencies),
`gangli-macos-universal`. They are not code-signed: Windows and macOS ask for a confirmation the first time.

Double click opens the window. From a terminal:

```
gangli network create family        # on your computer: prints the public and private codes
gangli node --code gangli-pub-…     # on the machine to manage (or: gangli install --code …)
gangli nodes                        # the machines
gangli exec nas -- df -h
gangli --help
```

## More

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): design and decisions.
- [AGENTS.md](AGENTS.md): how the project is built and tested (for people and AI agents).
- [docs/API.md](docs/API.md): the local API, for programs and AI agents.
- [docs/RELAY.md](docs/RELAY.md): running your own relay.
