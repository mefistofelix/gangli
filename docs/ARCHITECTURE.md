# gangli — architecture

Personal remote management of the owner's machines and those of family and friends. One static-ish binary
for Windows, Linux and macOS, three roles. No inbound ports anywhere, works through NAT, CGNAT, mobile and
slow networks, survives disconnects without the user noticing.

Decisions taken with the owner (2026-11-20) are marked **[decided]**. Things still to be verified in code
are marked **[verify]**.

## 1. Goals and non-goals

- The owner manages machines they are trusted with. **[decided]** No consent prompt, no on-screen indicator, no
  "pause" button on the managed side: relatives would break things. The binary is still not hidden (normal
  service name, normal install path, uninstall command): it is a management tool, not malware.
- Security of the owner's own keys (client key, master key) is the owner's responsibility **[decided]**; the
  software keeps them in restricted files and never prints them.
- UX first: double click, one elevation prompt, done. Reconnections are invisible.
- Everything usable by an LLM agent: CLI with `--json`, local API with a token granted once through the GUI.

## 2. One binary, three roles

| Role | Runs as | Needs install | What it does |
|---|---|---|---|
| **agent** (the "slave") | system service (root / SYSTEM) | yes | joins one or more networks, reports inventory, accepts sessions from the owner's clients, opens shells, forwards to its LAN |
| **master** (control center) | same system service | yes | holds a network's private key, admits agents, issues client certificates, keeps the node list, distributes updates and revocations |
| **client** | the user, no service | no | lists nodes, opens ssh / terminal / forwards / SOCKS, local SSH gateway, local API for scripts and LLMs |

Agent and master are **the same service** **[decided]**: `gangli install` installs one service per machine;
then networks are added to it, each with a role:

```
service state
└── networks
    ├── home      role = agent   (joined with a join code)
    ├── family    role = agent
    └── mine      role = master  (created here: holds the network private key)
```

A machine can be master of one network and agent of another. Without arguments the binary opens the unified
GUI (section 11); with a subcommand it is the CLI.

## 3. Identity, keys and what separates a master from an agent

Every install has a **node key** (Ed25519, which is also its iroh `EndpointId`), generated locally and never
leaving the machine.

**Network.** `gangli master create <name>` generates the **network key** (Ed25519). Its public half is the
network id. Its private half stays on the master and is what *makes* a master: the role is cryptographic, not
a flag **[decided: technically agreed]**. A second master (backup) is added by exporting a master bundle
(`gangli master export`, password protected), never through a join code.

**Join code** (what used to be the "network key" of a slave), e.g. `gangli1-k7d2…` (base32, ~80 chars, also as QR):

- network id (public key) → the agent pins it;
- EndpointId(s) of the master(s);
- an enrollment secret (128 bit), revocable, optional expiry and use count;
- optional default name / group for the node.

On join the agent connects to a master, proves the secret, and gets back a **membership certificate** signed
by the network key (its node key, name, network). Default: admitted automatically (UX); per code the owner
can choose "needs approval".

**Client pairing (once)** **[decided]**: on the master, `gangli master pair` (or the GUI button) shows a
one-time code; the client enters it, and gets a **client certificate** (role `owner`, signed by the network
key, renewed automatically). From then on the client talks to the master and to agents without asking again.

**Who can connect to what**

- Agents accept inbound connections only from peers presenting a valid client certificate of a network they
  belong to, not revoked. Agents never accept connections from other agents.
- Data goes **client ↔ agent directly** (end-to-end, master not in the path). The client caches the agent
  EndpointIds, so known agents stay reachable when the master is down.
- Revocations (clients, agents, join codes) are signed lists pushed by the master to every agent.

**Security implications (accepted)**

| Leak | Consequence | Mitigation |
|---|---|---|
| a join code (also: a binary with the code embedded) | someone can enroll a *fake node* in the list; it gets no access to anything else | expiry / use count on codes, revoke, "needs approval" codes, node list shows new nodes |
| an agent's machine | that machine only; it cannot become master or reach other agents | revoke its certificate |
| the client key | full access to every agent of its networks | owner's responsibility; revoke from the master |
| the master key | full control of that network | owner's responsibility; offline backup via `master export` |

## 4. Transport: iroh

- **iroh 1.x** (stable since June 2026, Apache-2.0/MIT) as a library: dial by public key, QUIC (noq),
  hole-punching first, relay fallback, **QUIC multipath** (direct and relay paths alive at once, hot swap
  between Wi-Fi and mobile).
- Relays and discovery: **public n0 relays and DNS discovery by default** **[decided]**, overridable in the
  config (`relays = [...]`, own `iroh-relay`, own discovery). Relays only see encrypted traffic and the
  endpoint ids. On UDP-blocked networks the relay path is HTTPS/WebSocket on TCP 443, which covers the
  "TCP fallback" osewa implements on its own.
- One ALPN per protocol: `gangli/ctrl/1` (control, inventory, tickets), `gangli/stream/1` (resumable
  streams), `gangli/term/1` (persistent terminal).
- **Unknown peers are rejected during the handshake**, before any application byte (allowlist of
  EndpointIds / certificates).

**Hiding the master** — with iroh the master needs **no open port** either: it only uses outbound UDP like
the agents and is reached by EndpointId through the relay or hole-punching. A port scan of its network finds
nothing; a peer that knows its EndpointId but has no certificate is dropped at the handshake. The only public
pieces are n0's relays (or the owner's, if configured).

## 5. Reliability on bad networks (core of the project)

Three layers, each one independent:

1. **Path (iroh).** Multipath, migration, keepalives. Short drops and network changes are absorbed here.
2. **Resumable streams (ours).** Every forwarded stream (ssh, TCP forward, SOCKS, exec) has a session id; both
   ends keep the bytes the other side has not acknowledged in a ring buffer (default 8 MiB per stream,
   configurable). When the iroh connection dies, the client redials at once (and on every network-change
   event) with backoff, sends `RESUME(session, received_offset)`, both sides replay what is missing. **The local
   TCP connections (PuTTY ↔ client, agent ↔ embedded sshd or LAN target) never see the outage**, so an SSH
   session survives minutes or hours of disconnection (grace period, default 24 h for interactive sessions).
   Same idea as Eternal Terminal, generic for any TCP protocol.
3. **Persistent terminals (`gangli term`).** The PTY lives in the agent, detached from any connection. A
   host-side VT engine keeps the screen state; on (re)attach the agent sends a screen snapshot and then live
   output. On slow links it switches to **state sync** (mosh-like: send the latest screen diff, skip
   intermediate frames), and later **predictive local echo**. This is what osewa and shpool do; it makes a
   `cat bigfile` on GPRS harmless and the terminal usable at 1 s RTT.

Also: zstd on bulk streams, interactive frames prioritized over bulk, `exec` jobs keep running and buffering
their output while the client is away (`gangli job output <id>`).

## 6. SSH

- The agent embeds an SSH server (**russh**) that listens on nothing: it is only reachable over a
  `gangli/stream/1` stream. Works on Windows without OpenSSH (ConPTY). SFTP subsystem (russh-sftp) for
  WinSCP / `scp`.
- Authentication already happened at the iroh layer (client certificate); inside the tunnel the SSH username
  only selects the **OS user**. The host key is derived from the node key and the client writes `known_hosts`
  itself: no prompts.
- On the client, three ways:
  - **`~/.ssh/config`** (`gangli ssh-config --write`): `Host nas.gangli` with `ProxyCommand gangli proxy %h`,
    so `ssh root@nas.gangli` works from any OpenSSH and any agent tool;
  - **local SSH gateway** on `127.0.0.1:2222`: `ssh -p 2222 'root@nas'@127.0.0.1` (user@node in the SSH
    username), one port for PuTTY / WinSCP / anything;
  - **per-node local port** (`gangli forward nas`) and the GUI button "Open PuTTY" (`putty -P <port>
    user@127.0.0.1`, or another configured client).
- Which users: what the OS allows **[decided]**.
  - Unix, service as root: any local user (setuid/setgid/initgroups, login shell). Shown as "full access".
  - Windows, service as SYSTEM: SYSTEM shell; the logged-in user's session through its token
    (`WTSQueryUserToken`); another user only with its password (`LogonUser`).
  - Service as a normal user (no elevation possible): only that user.

## 7. Their LAN

Always on **[decided]**: every agent is a hop into its LAN.

- `gangli forward nas 192.168.1.1:80` → local port; `gangli socks nas` → SOCKS5 on `127.0.0.1`.
- The agent reports interfaces, gateway, DNS and the neighbour table (devices seen on the LAN), and can run a
  quick scan on request.

## 8. Inventory

Per node: name, hostname, OS and version, arch, local IPs, public IP (as observed by iroh), connection type
(direct / relay, RTT), RAM, disks, CPU, uptime, logged-in users, local users, the account the service runs as
("full access" when root / SYSTEM), agent version, last seen. Crate `sysinfo` plus small per-OS bits.

Names: the master assigns a simple unique name per network (default: slug of the hostname, renameable);
addressing is `name` or `name.network` when ambiguous.

## 9. Install, elevation, embedded join code

- `gangli install [--join CODE]` copies the binary to a fixed place and registers the service:
  - Windows: `%ProgramFiles%\gangli\gangli.exe`, SCM service (`windows-service`), state in `%ProgramData%\gangli`;
  - macOS: `/usr/local/bin/gangli`, LaunchDaemon, state in `/Library/Application Support/gangli`;
  - Linux: `/usr/local/bin/gangli`, systemd unit, state in `/var/lib/gangli`.
  State files are readable by root / SYSTEM / Administrators only.
- Elevation is requested by the binary itself when needed, one prompt: Windows UAC (`ShellExecute runas`),
  macOS `osascript … with administrator privileges`, Linux `pkexec` then `sudo`.
- `gangli uninstall` removes service, files and state. `gangli join CODE` / `gangli leave NET` talk to the
  running service over local IPC (named pipe / unix socket, admins only).
- **Embedded join code** without recompiling: a trailer at the end of the file
  (`payload | u32 length | "GANGLI-CFG1"`). `gangli pack --code CODE` (client or master, GUI button too)
  makes personalized copies of the release binaries for each OS. The embedded code is only the *default* of
  `--join`. Double click on such a binary → GUI with one button "Install and connect" → one elevation prompt
  → done.
  - Windows: appended data to an unsigned PE is fine.
  - macOS arm64 needs a valid (ad-hoc) signature: `pack` re-signs ad hoc in-process (`apple-codesign`)
    **[verify early]**; fallback: code in a `gangli.join` file next to the binary.
  - Binaries are unsigned; the owner guides relatives through SmartScreen / Gatekeeper the first time
    **[decided]**.

## 10. Updates

The master distributes updates to its agents (agents never need GitHub access). Releases are signed with
the owner's release key (Ed25519, kept in `.tools/`); agents verify the signature, swap the binary
atomically, restart, and roll back if the new version does not come back healthy within 60 s. The master
gets new releases from this private repository (token in its config) or by `gangli release push`.

## 11. GUI, local API, LLM agents

- **Native GUI** (egui / eframe) **[decided: native, not browser]**, one window, three tabs:
  - *This computer*: service install / uninstall, networks joined, status;
  - *Control center*: networks, join codes (create, QR, revoke), nodes (approve, rename, revoke), pairing
    codes, `pack` binaries;
  - *Client*: node list with inventory, buttons ssh / terminal / PuTTY / forward / SOCKS, API tokens.
  The GUI runs as the user; admin actions go to the service via IPC.
- The **client process** runs in the background (started by the GUI or on demand by the CLI) and owns the
  forwards, the SSH gateway, the resumable streams and the local API.
- **Local API**: HTTP/JSON on `127.0.0.1` (plus named pipe / unix socket); address in a discovery file in the
  user's config dir. Auth with bearer tokens.
- **Token for an LLM, no password** **[decided]**: `gangli api request --name claude --scope exec` → the GUI
  pops up "Allow *claude* (scope exec)? Allow / Deny" (native dialog if the GUI is closed) → the token is
  returned to the caller, which keeps it locally (file in its own config dir) and works from then on. Tokens
  are listed and revocable in the GUI. Scopes: `read`, `connect`, `exec`, `admin`. Headless fallback:
  `gangli api approve <code>` from another terminal.
- **CLI** mirrors the API: `nodes`, `info`, `exec`, `ssh`, `term`, `cp`, `forward`, `socks`, `job`, all with
  `--json` and stable exit codes, so an LLM can drive everything (`gangli exec nas -- df -h`).

## 12. Platforms and build

- Rust stable. Targets: `x86_64-pc-windows-msvc` (static CRT), `x86_64` and `aarch64-unknown-linux-gnu` built
  with cargo-zigbuild against **glibc 2.28** (runs on any distro since 2018; the GUI libraries — X11 /
  Wayland / GL — are loaded at runtime, so headless servers run the CLI and the service without them),
  macOS universal (`aarch64` + `x86_64`, ad-hoc signed). Single file per OS.
- Built on the owner's blind build machine `mefistofelix/build-box`; see AGENTS.md.

## 13. Libraries (researched 2026-11)

| Need | Choice | Notes |
|---|---|---|
| P2P transport | `iroh` 1.x | stable, multipath, self-hostable relay; `pigeons` (same authors) is the reference for "ssh over iroh" and service install |
| SSH server / SFTP | `russh`, `russh-sftp` | maintained (0.6x); pick a version without RUSTSEC-2026-0007 |
| VT engine | `alacritty_terminal` first, behind a `VtEngine` trait | pure Rust, used by Zed, no extra toolchain; **libghostty-vt** (C ABI 1.0 since Nov 2026, `ghostty-vt-sys`) is more complete but needs Zig and has MSVC link quirks: evaluate as the second engine |
| PTY | thin own layer: `rustix` openpty + ConPTY via `windows-sys` | `portable-pty` works but its last release is Feb 2025; can start with it |
| Session persistence design | study `shpool` (+ `shpool-vterm`), `osewa`, `RoSE` | shpool: detached shells + screen restore; osewa: QUIC/WS racing, predictive echo; RoSE: mosh-like over quinn |
| Inventory | `sysinfo` | |
| Service | `windows-service`, launchd plist, systemd unit | |
| GUI | `eframe` / `egui` | |
| Async | `tokio` | |
| macOS re-sign | `apple-codesign` | for `pack` |

Code from osewa / pigeons can be read and adapted (Apache-2.0 / MIT, keep notices); no external binaries are
called.

## 14. Implementation notes (phase 3, 2026-11)

- **iroh 1.5** with `presets::N0` (n0 relays and address lookup); one endpoint per process: the service uses
  the node key, the client its own `client.key`.
- **Join / pairing code** = kind byte + master endpoint id + 16-byte secret (`gangli-` + 79 base32 chars).
  The code does not carry the network id: the master id is the trust anchor (iroh authenticates it), and the
  master answers the join with the network id, the node's certificate and its own master certificate, which
  the agent / client verify and pin.
- **Certificates** are signed by the network key over a domain-separated JSON body: agent 10 years, client
  1 year (renewal comes with phase 8), master 10 years.
- **CLI ↔ service** go through the state file, not a socket: elevated commands change `state.json` under a
  cross-process lock (`File::lock`), the service rereads it every 2 s. The named pipe / unix socket of
  section 9 arrives with the GUI, which needs live answers.
- **Windows elevation** relaunches hidden with UAC and relays the elevated copy's output into the same
  terminal (temp file), so `gangli master code home` prints the code where it was typed.
- **State dir ACL on Windows**: inheritance removed, SYSTEM + Administrators only (`icacls`), because
  ProgramData is readable by every user.
- **Known limits**: a master machine cannot (yet) join its own network as an agent (it would have to call
  itself); revocation lists and agent-side allowlists of client ids are not pushed yet: an agent accepts
  any client certificate signed by the network key.

## 15. Roadmap

1. Skeleton, build on build-box, release pipeline. ✓
2. Service install / uninstall / elevation on the three OSes. ✓ (IPC and embedded trailer moved to 6 and 8)
3. iroh endpoints, network create, join code, client pairing, certificates, node list + inventory. ✓
4. Resumable streams; `forward`, `socks`, `proxy`, SSH config writer.
5. Embedded SSH server (users, Windows tokens), SSH gateway, SFTP.
6. GUI (three tabs), local API + token approval, PuTTY launcher.
7. `exec` / jobs, `gangli term` with VT engine, state sync, predictive echo.
8. Signed self-update through the master; revocation lists; `pack`.
