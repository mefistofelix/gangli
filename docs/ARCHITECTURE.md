# gangli — architecture

Personal remote management of the owner's machines and those of family and friends. One binary for Windows,
Linux and macOS, two roles. No inbound ports anywhere, no server to run, works through NAT, CGNAT, mobile and
slow networks, survives disconnects without the user noticing.

Decisions taken with the owner are marked **[decided]**. Things still to be verified in code are marked
**[verify]**.

## 1. Goals and non-goals

- The owner manages machines they are trusted with. **[decided]** No consent prompt, no on-screen indicator, no
  "pause" button on the managed side: relatives would break things. The binary is still not hidden (normal
  service name, normal install path, uninstall command): it is a management tool, not malware.
- Security of the owner's own devices and keys is the owner's responsibility **[decided]**; the software keeps
  keys in owner-only files and never prints them.
- **Portable** **[decided]**: everything lives next to the executable in `gangli-data/` (node key and
  memberships, console key, networks, API, logs), never in system or user folders. The only things written
  elsewhere are the OS's own service registration (SCM entry, systemd unit, LaunchDaemon plist) while the
  service is installed.
- UX first: double click, one elevation prompt, done. Reconnections are invisible.
- The node list is always current without waiting, with as little traffic as possible on the managed
  machines (no polling) **[decided]**.
- Everything usable by an LLM agent: CLI with `--json`, local API with a token granted once through the GUI.

## 2. Roles and names **[decided 2026-11-20]**

No control center: with iroh nobody needs to be reachable to be dialed, so a rendezvous server adds nothing
to connectivity, and its other jobs are done by the consoles.

| Name | What | Runs as | Install |
|---|---|---|---|
| **node** | a managed machine | the user (`gangli node`, or just running a binary with the code embedded), or the system service (`gangli install`) for full access and start at boot | optional |
| **console** | where the owner works: GUI with tray icon, CLI, local API | the user, only while it is open — **never a service or a background daemon** | no |
| **hub** | not a role: a console left open on an always-on machine (home box, VPS), for an always-current view and automations while the owner's PC is off | the user | no |

One unified GUI (section 12): *this computer* can be a node (enter or use the embedded public code, start,
optionally install the service) and a console (create a network, enter a private code, machines list) at
the same time. With a subcommand the binary is the CLI.

CLI names: node side `node`, `install`, `uninstall`, `join`, `leave`, `status`; console side
`network create|add|codes|remove`, `nodes`, `info`, `forget`, `console run|stop|status`; `pack`.

## 3. Identity, keys, codes **[decided 2026-11-20]**

- **A network is its key** (Ed25519). `gangli network create home` (or the GUI) makes one and shows two
  codes:
  - **public code** `gangli-pub-…` (90 chars): network id + join secret. For the machines to manage, also
    embedded in a binary (`gangli pack`). It lets a machine become a node, nothing more. Fixed per network.
  - **private code** `gangli-key-…`: the network key itself (with a checksum against typos). Pasted into
    a console (`gangli network add`, or the GUI) it makes that console an owner: **full control**. All
    consoles holding it are equal; there is no primary.
- The join secret and the directory key are derived from the network key (deterministic Ed25519 signatures
  over fixed labels), so every owner console derives the same values.
- Every node has a **node key**, every console a **console key** (also their iroh endpoint ids), generated
  locally, never leaving the machine. Nodes and consoles get a **certificate** signed by the network key:
  the admitting console signs the node's (name, 10 years), each console signs its own.
- **Admission**: a node with the public code looks the network's directory up (section 5), knocks on a
  console with the join secret; the console signs its certificate and answers with name, certificate and
  console list. Until a console is open the node keeps knocking (backoff up to 30 s): a new machine shows up
  as soon as the owner opens a console.

**Who can talk to whom**

- Nodes accept only consoles presenting a console certificate of one of their networks that names the
  connecting endpoint. Nodes never talk to other nodes.
- Consoles accept knocks from nodes with a valid node certificate or with the join secret, and sync with
  consoles presenting a console certificate.
- All traffic is end to end between console and node.

**Security implications (accepted, owner's responsibility)**

| Leak | Consequence | Mitigation |
|---|---|---|
| the public code (also: a binary carrying it) | someone can add a *fake node* to the list; it gets no access to anything; it can read the directory's console ids | `forget`; new machines are visible at once; a new public code means a new network (rotation: later) |
| a node's machine | that machine only | `forget` |
| the private code / a console's user profile | full control of the network, no revocation possible | keep it like a password manager master key; if it leaks, create a new network and move the machines |

## 4. Transport: iroh

- **iroh 1.5** as a library: dial by public key, QUIC, hole-punching first, relay fallback, **QUIC
  multipath** (direct and relay paths alive at once, hot swap between Wi-Fi and mobile).
- **Public n0 relays and address lookup by default** (`presets::N0`) **[decided]**, overridable later in the
  config. Relays only see encrypted traffic and endpoint ids. On UDP-blocked networks the relay path is
  HTTPS/WebSocket on TCP 443, which covers the "TCP fallback" osewa implements on its own.
- **n0 public relays are for development**: the iroh docs advise against them in production; they are shared,
  rate limited and without guarantees, and the address lookup (`dns.iroh.link`) is an n0 service too.
  Presence traffic is tiny, but when hole-punching fails (CGNAT, symmetric NAT, some mobile networks) all
  session data goes through the relay. Before rolling out to relatives: own `iroh-relay` + `iroh-dns-server`
  on a small VPS (public IP and DNS name: the only open port of the whole system, and not on any managed or
  owner machine), configured with `RelayMode::Custom`, n0 kept as fallback. Paid n0 shared relays are the
  alternative.
  Options researched for later, free ones included: [RELAY.md](RELAY.md).
- Dialing by id alone works through n0 address lookup: verified on 2026-11-20 (join 3 s, list and direct
  query ≈ 1.4 s including endpoint start, direct path 7 ms on a LAN).
- ALPN `gangli/2`: length-prefixed JSON frames; resumable streams and terminals get their own ALPNs.
- Nothing listens on a port: there is nothing to scan, and a peer without a certificate gets an error
  answer and the connection is closed.

## 5. Presence: a live list without polling **[decided]**

- An open console keeps **one live session per node** (a QUIC connection with a session stream). A node is
  online exactly while its session is up; the list the GUI / CLI show comes from memory, instantly
  (measured: `gangli nodes` 25 ms, `info` 120 ms, a restarted node back online in 3 s).
- **Who opens it**: the console dials every known node when it opens (in parallel) and reconnects quickly
  after a drop (3 s, doubling up to 60 s for 10 minutes). The node **knocks** on its consoles when it
  starts, when its LAN addresses change, and 5 s after a session drops: a machine that comes online
  appears at once. Offline nodes are retried by the console only every 5 minutes as a safety net; those
  attempts never reach the node (it is off).
- **Idle cost**: per online node a 50-byte ping every 25 s. The node checks its own inventory every minute
  *locally* and pushes it only when its stable part (OS, IPs, users, disks, memory size, version) changed.
  Changing values (free memory, free disk, uptime) are asked for on demand (`gangli info`).
- **Directory**: one pkarr record per network, signed by the network key, on the Mainline DHT and pkarr
  HTTP relays (no server of ours): network name, up to 3 console ids in clear (for nodes), up to 8 node ids
  encrypted with the derived directory key (for consoles). Consoles republish it when it changes and every
  30 minutes while open. It is read on events only: a console that opens (or gets a private code) reads it
  and syncs node, console and removed lists with the consoles listed there; a node reads it while waiting
  for admission, and once when none of its known consoles answers at start. More than 8 nodes: the others
  come from the consoles' sync (pages under derived keys later).
- **CLI with no open console**: the command runs a one-shot console in its own process (dials the machines
  in parallel, reads the directory, a few seconds), then exits. With an open console the CLI asks it over
  127.0.0.1 with a token from an owner-only file and answers at once. Nothing is ever left running.

## 6. Reliability on bad networks

Three layers, each one independent:

1. **Path (iroh).** Multipath, migration, keepalives. Short drops and network changes are absorbed here.
2. **Resumable streams (phase 4).** Every forwarded stream (ssh, TCP forward, SOCKS, exec) has a session id;
   both ends keep the bytes the other side has not acknowledged in a ring buffer (default 8 MiB per stream).
   When the connection dies, it is redialed at once (and on every network-change event), `RESUME(session,
   received_offset)`, both sides replay what is missing. **The local TCP connections (PuTTY ↔ console, node ↔
   embedded sshd or LAN target) never see the outage**, so an SSH session survives minutes or hours of
   disconnection (grace period, default 24 h). Same idea as Eternal Terminal, generic for any TCP protocol.
3. **Persistent terminals (`gangli term`, phase 7).** The PTY lives in the node, detached from any
   connection. A host-side VT engine keeps the screen state; on (re)attach the node sends a screen snapshot and
   then live output. On slow links it switches to **state sync** (mosh-like: latest screen diff, skip
   intermediate frames), then **predictive local echo**. What osewa and shpool do; `cat bigfile` on GPRS is
   harmless and the terminal stays usable at 1 s RTT.

Also: zstd on bulk streams, interactive frames before bulk, `exec` jobs keep running and buffering their output
while the console is away (`gangli job output <id>`).

## 7. SSH and terminals

- The node embeds an SSH server (**russh**) that listens on nothing: it is only reachable through a gangli
  stream. Works on Windows without OpenSSH. SFTP subsystem (russh-sftp) for WinSCP / `scp`.
- **PTY**: Unix `openpty`; Windows **ConPTY**, the native pseudo-console API (`CreatePseudoConsole`, Windows
  10 1809+), called through `windows-sys`: no extra dependency. Needed only for interactive sessions;
  `gangli exec` uses plain pipes.
- Authentication already happened at the iroh layer (console certificate); inside the tunnel the SSH username
  only selects the **OS user**. The host key is derived from the node key and the console writes `known_hosts`
  itself: no prompts.
- On the console, three ways:
  - **`~/.ssh/config`** (`gangli ssh-config --write`): `Host nas.gangli` with `ProxyCommand gangli proxy %h`,
    so `ssh root@nas.gangli` works from any OpenSSH and any agent tool;
  - **local SSH gateway** on `127.0.0.1:2222`: `ssh -p 2222 'root@nas'@127.0.0.1`, one port for PuTTY / WinSCP;
  - **per-node local port** (`gangli forward nas`) and the GUI button "Open PuTTY".
- Which users: what the OS allows **[decided]**.
  - Unix, service as root: any local user (setuid/setgid/initgroups, login shell). Shown as "full access".
  - Windows, service as SYSTEM: SYSTEM shell; the logged-in user's session through its token
    (`WTSQueryUserToken`); another user only with its password (`LogonUser`).
  - Service as a normal user (no elevation possible): only that user.

## 8. Their LAN

Always on **[decided]**: every node is a hop into its LAN.

- `gangli forward nas 192.168.1.1:80` → local port; `gangli socks nas` → SOCKS5 on `127.0.0.1`.
- The node reports interfaces, gateway, DNS and the neighbour table, and can run a quick scan on request.

## 9. Inventory

Per node: name, hostname, OS and version, arch, LAN IPs, link (direct / relay, RTT, remote address), RAM, disks,
CPU, uptime, local users, the account the service runs as ("full access" when root / SYSTEM), gangli
version, online since / last seen. Crate `sysinfo`, local users from `/etc/passwd` on Linux.

Names: the admitting console names nodes (slug of the hostname, unique per network, written in their certificate); addressing is `name` or
`name.network` when ambiguous.

## 10. Node: run, install, elevation, embedded code

- A node needs no install: `gangli node --code …` (or running a binary that carries the code) runs it as
  the user, access limited to that user, no elevation.
- `gangli install [--code CODE]` registers the service on **the binary where it is** (no copy elsewhere),
  same `gangli-data/node`, so the node keeps its identity: SCM service (Windows, LocalSystem), systemd unit
  (Linux), LaunchDaemon (macOS). Because the service runs as SYSTEM / root, the folder must not be writable
  by ordinary users (anyone could swap the binary): install requires a folder of its own (e.g. `C:\gangli`,
  `/opt/gangli`) and makes it writable by administrators only, `gangli-data/node` readable by them only,
  `gangli-data/console` still the installing user's. `uninstall` gives the folder back and keeps the
  identity (delete the folder to remove everything).
- Elevation is requested by the binary itself, one prompt: Windows UAC (relaunch hidden, its output relayed
  into the same terminal), macOS `osascript … with administrator privileges`, Linux `pkexec` then `sudo`.
- `gangli join` / `leave` / `status` change the node state file under a cross-process lock (elevated only
  when the service is installed); the node rereads it every 2 s.
- **Embedded public code** without recompiling: a trailer at the end of the file (`code | u32 length |
  "GANGLI-CODE1"`), or a `gangli.code` file next to the binary. `gangli pack --code … --from <release> --out
  <file>` makes personalized copies for each OS. Double click → GUI: "Start" (as this user) or "Install"
  (one elevation prompt) → done.
  - Windows: appended data to an unsigned PE is fine.
  - macOS arm64 needs a valid (ad-hoc) signature: `pack` re-signs ad hoc in-process (`apple-codesign`)
    **[verify early]**; fallback: code in a file next to the binary.
  - Binaries are unsigned; the owner guides relatives through SmartScreen / Gatekeeper the first time
    **[decided]**.

## 11. Updates

Releases are signed with the owner's release key (Ed25519, kept in `.tools/`). Nodes get updates from the
consoles over the session (the console downloads the release once), verify the signature, swap the binary
atomically, restart, and roll back if the new version does not come back healthy within 60 s.

## 12. GUI, tray, local API, LLM agents

- **Native GUI** (egui / eframe) **[decided: native, not browser]**, one window, unified:
  - *This computer* (node): public code (typed or embedded), start / stop, install / uninstall the service,
    networks and admission state;
  - *Networks* (console): create a network (shows public and private codes, QR), add one with a private
    code, `pack` binaries for relatives;
  - *Machines* (console): live list, inventory, buttons ssh / terminal / PuTTY / forward / SOCKS;
  - *API tokens*.
- **Link quality in the GUI**: every machine shows whether its traffic goes **direct** (hole-punching worked)
  or through a **relay** (slower, rate limited on the public relays), with the RTT; the CLI has the same in
  the `LINK` column.
- **Tray icon** while open (closing the window keeps it in the tray): online machines count, open, quit.
  `tray-icon` on Windows / macOS, `ksni` (pure Rust D-Bus) on Linux: nothing linked to GTK, so the Linux
  binary stays a single file that loads the GUI libraries at runtime. Starting at login is an option the
  user turns on, not a service.
- **Local API** **[decided]**: off by default, enabled from the GUI or `gangli api enable` (127.0.0.1, port 7373),
  served while a console is open. Named tokens made and revoked from the GUI or `gangli api token add|remove`,
  never through the API itself; scopes `read` (status, machines, details) and `admin` (also networks, removing
  machines), later `exec` / `connect`. Routes: `POST /v1/op` with an operation as JSON (`{"op":"nodes"}`),
  `GET /v1/status`, `GET /v1/nodes`, `GET /v1/nodes/<name>`; `Authorization: Bearer <token>`.
- The CLI and GUI talk to the open console through the same local HTTP with an internal token (new at every
  start, in `gangli-data/console/console.port`), so an LLM using the CLI needs no token at all.
- **CLI** mirrors the API: `nodes`, `info`, `exec`, `ssh`, `term`, `cp`, `forward`, `socks`, `job`, all with
  `--json` and stable exit codes (`gangli exec nas -- df -h`).

## 13. Platforms and build

- Rust stable. Targets: `x86_64-pc-windows-msvc` (static CRT), `x86_64` and `aarch64-unknown-linux-musl`
  **fully static** (cargo-zigbuild; no glibc or any other library needed) **[decided]**, macOS universal
  (ad-hoc signed). Single file per OS. No `libc` crate either: root detection reads `/proc/self/status`
  (Linux) or a fresh file's owner (macOS), signals go through tokio.
- **Linux GUI in a static binary** (phase 6): a static binary cannot `dlopen` the system's X11 / Wayland / GL
  libraries, so the GUI speaks the protocols itself: X11 through `x11rb`'s pure-Rust connection (also
  XWayland on Wayland desktops), egui drawn by a CPU renderer into an X11 image, keyboard from the X11
  keymap, tray through `ksni` (pure-Rust D-Bus). No winit / GL on Linux; Windows and macOS use eframe
  normally. **[verify]** in phase 6.
- Built on the owner's blind build machine `mefistofelix/build-box`; see AGENTS.md.

## 14. Libraries (researched 2026-11)

| Need | Choice | Notes |
|---|---|---|
| P2P transport | `iroh` 1.5 | multipath, self-hostable relay; `pigeons` (same authors) is the reference for "ssh over iroh" |
| SSH server / SFTP | `russh`, `russh-sftp` | maintained (0.6x); pick a version without RUSTSEC-2026-0007 |
| VT engine | `alacritty_terminal` first, behind a `VtEngine` trait | pure Rust, used by Zed; **libghostty-vt** (C ABI 1.0, `ghostty-vt-sys`) is more complete but needs Zig and has MSVC link quirks |
| PTY | `rustix` openpty + ConPTY via `windows-sys` | `portable-pty` (the usual wrapper) has had no release since Feb 2025 |
| Session persistence design | study `shpool` (+ `shpool-vterm`), `osewa`, `RoSE` | shpool: detached shells + screen restore; osewa: QUIC/WS racing, predictive echo; RoSE: mosh-like over quinn |
| Inventory | `sysinfo` 0.40 | |
| Directory | `pkarr` 9 (Mainline DHT + HTTP relays), `chacha20poly1305` | 1.5M downloads; RustCrypto AEAD, 120M downloads |
| Tray | `tray-icon` (Windows, macOS), `ksni` (Linux) | no GTK linking |
| Service | `windows-service`, launchd plist, systemd unit | |
| GUI | `eframe` / `egui` | |
| Async | `tokio` | |
| macOS re-sign | `apple-codesign` | for `pack` |

Code from osewa / pigeons can be read and adapted (Apache-2.0 / MIT, keep notices); no external binaries are
called for core features.

## 15. Known limits (now)

- The private code cannot be revoked: a leak means a new network.
- The directory lists up to 8 nodes; a console that only has the private code and finds no other console
  open sees those 8 first, the others once it syncs with another console.
- The public code is fixed per network (no expiry or use count).
- A machine can be node and console at the same time (separate keys); the console does not list its own
  node specially.

## 16. Roadmap

1. Skeleton, build on build-box, release pipeline. ✓
2. Service install / uninstall / elevation on the three OSes. ✓
3. iroh, networks as keys, public / private codes, directory on pkarr, certificates, live sessions,
   inventory push, node without install, embedded code, one-shot or open console. ✓
4. Resumable streams; `forward`, `socks`, `proxy`, SSH config writer.
5. Embedded SSH server (users, Windows tokens), SSH gateway, SFTP.
6. GUI with tray (node + console unified), local HTTP API + token approval, PuTTY launcher.
7. `exec` / jobs, `gangli term` with VT engine, state sync, predictive echo.
8. Signed self-update over the sessions; directory pages for more than 8 nodes; public code rotation; own relay.
