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
- **Portable** **[decided]**: everything lives next to the executable (two files, below), never in system or user folders. The only things written
  elsewhere are the OS's own service registration (SCM entry, systemd unit, LaunchDaemon plist) while the
  service is installed.
- **Two files only** **[decided 2026-11-20]**, next to the binary, TOML:
  - `gangli.node` — the node: its key, the networks it belongs to (public code, certificate, consoles),
    service settings (installed, account);
  - `gangli.console` — everything else: console key, networks with their private keys and machines, other
    consoles, local port and token for the CLI, API and tokens, SSH gateway and login rules, launchers,
    tunnels.
  gangli rewrites them when its state changes (keeping a header that explains them; other comments are not
  kept) and picks up hand edits within 2 seconds. Both hold secrets: owner-only files (admin-only for
  `gangli.node` once the service is installed). Besides them only `gangli.log` (node log, size-capped).
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
  codes, each a bare base32 string, no prefix **[decided 2026-11-21]** (a first byte inside tells them apart
  and `decode` refuses one where the other is expected; the public one is the longer of the two):
  - **public code** (79 chars): network id + join secret. For the machines to manage, also embedded in a
    binary (`gangli pack`). It lets a machine become a node, nothing more. Fixed per network.
  - **private code** (60 chars): the network key itself (with a checksum against typos). Pasted into a
    console (`gangli network add`, or the GUI) it makes that console an owner: **full control**. All consoles
    holding it are equal; there is no primary.
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
- Own relays are a setting of each file: `relays = ["https://relay.example.org"]` in `[node]` of `gangli.node`
  and in `[console]` of `gangli.console`, `relays_n0_fallback = true` (default) to keep the n0 ones as well;
  applied when the node / console starts.
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
- **Idle cost**: per online node a 50-byte ping every 25 s. Inventory is **never published anywhere**: a
  console asks a node for it over their authenticated session (the node pushes its stable part — OS, IPs,
  users, disks, memory size, version — when it changes, and the changing values, free memory / disk / uptime,
  on demand for `gangli info`). So everything descriptive about a machine lives only between that machine and
  an owner console, never on the DHT.
- **Directory** — **only what is needed to connect, nothing descriptive** **[decided 2026-11-21]**: one pkarr
  record per network, signed by the network key, on the Mainline DHT and pkarr HTTP relays (no server of
  ours). It holds **no network name and no inventory**, only ids: up to 3 console ids in clear (a node needs
  them to find a console to knock), the first 8 node ids encrypted with the derived directory key (so only an
  owner console sees which machines exist), and the number of pages: further records under keys derived from
  the network key, 12 more encrypted node ids each (up to 16 pages). Nodes cannot write it (they lack the
  network key); a console publishes a node's id there only after the node has knocked and been admitted.
  Consoles republish it when it changes and every 30 minutes while open. It is read on events only: a console
  that opens (or gets a private code) reads it, learns which nodes exist, dials them and gets everything else
  live; a node reads it while waiting for admission, and once when none of its known consoles answers at
  start. The network's name is a console-side label, not in the directory.  
- **CLI with no open console**: the command runs a one-shot console in its own process (dials the machines
  in parallel, reads the directory, a few seconds), then exits. With an open console the CLI asks it over
  127.0.0.1 with a token from an owner-only file and answers at once. Nothing is ever left running.

## 6. Reliability on bad networks

Three layers, each one independent:

1. **Path (iroh).** Multipath, migration, keepalives. Short drops and network changes are absorbed here.
2. **Resumable streams** (implemented, `resume.rs`; verified 2026-11-20: an SSH session printing a count kept
   every line, in order, across a forced cut of the connection).** Every forwarded stream (ssh, TCP forward, SOCKS, exec) has a session id;
   both ends keep the bytes the other side has not acknowledged in a ring buffer (default 8 MiB per stream).
   When the connection dies, it is redialed at once (and on every network-change event), `RESUME(session,
   received_offset)`, both sides replay what is missing. **The local TCP connections (PuTTY ↔ console, node ↔
   embedded sshd or LAN target) never see the outage**, so an SSH session survives minutes or hours of
   disconnection (grace period, default 24 h). Same idea as Eternal Terminal, generic for any TCP protocol.
3. **Persistent terminals** (`term.rs`). Log in as `user+machine+name` (or `gangli open <machine> --term
   <name>`): the shell of terminal `name` lives on the node, detached from any connection, like a tmux
   session; `gangli term list|kill`. alacritty_terminal keeps its screen; whoever attaches gets it redrawn
   exactly, then the live output. Each viewer has a small queue: when the link is too slow and it fills up,
   the intermediate output is dropped and the viewer gets the current screen instead (**state sync**, as
   mosh does), so `cat bigfile` on a mobile link costs one screen. Predictive local echo needs our own
   terminal client: the GUI's integrated terminal has it (section 12); PuTTY / OpenSSH cannot.

**For scripts and LLMs**: `gangli exec <machine> [--user U] -- command` runs a command and returns its stdout,
stderr and exit code (`--json`: one object; API `{"op":"exec",…}`, token scope `exec`), over a resumable
stream. `gangli job start|list|output [--follow]|kill` runs commands in the background on the node: they keep
running, and keep their latest output (1 MiB), with no console connected; finished jobs are forgotten after
a day. API token scopes: `read` < `exec` (commands, jobs, terminals) < `admin`.

## 7. SSH gateway, terminal, launchers, tunnels **[decided 2026-11-20]**

### Local SSH gateway (console side)

**Status (2026-11-20):** implemented and verified on Windows with the OpenSSH client: key and password
logins, `user+machine`, commands with exit codes, interactive PowerShell in ConPTY, SFTP upload / download,
`ssh -L`. Not verified yet: Linux / macOS nodes (built and unit-tested only), the `desktop` user through a
SYSTEM service, the PuTTY / WinSCP launchers. Sessions and forwards are resumable streams: they survive
dropped connections (grace: 24 h for sessions, 15 min for forwards).

**Why the SSH server is on the console, not on each node** **[decided, reaffirmed 2026-11-21]**: the node
speaks only gangli's own protocol over iroh and runs no SSH server; the console terminates SSH locally and
opens a gangli session (`Req::Shell` / forward) to the node. The alternative — an sshd on every node, reached
through an iroh-forwarded port — was rejected:
- **One binary, every OS.** No dependency on an sshd being installed and configured on the node (Windows
  especially); the node spawns the PTY itself (`proc.rs`: ConPTY / `openpty`) and impersonates the right user
  with OS tokens / uid, the same way everywhere. Login rules, launchers and the `user+machine` convention
  live in one place (the console), not in an `authorized_keys` on each machine.
- **No inbound port on the node**, and nothing to brute-force there: the node accepts only an authenticated
  gangli peer (a console certificate). SSH auth is just a local gate between the client and the console on
  `127.0.0.1`.
- **Invisible reconnection.** The gangli session is a resumable stream: a dropped iroh connection is repaired
  under the client, the SSH session survives. A real sshd on the node has no session resume — a drop would
  kill it, breaking a core goal (mobile / flaky links).
- The one trade-off: it is not end-to-end SSH, so the console sees the session in clear. That is fine — the
  console **is** the owner (it holds the private key, it is your trust anchor), and the console↔node hop is
  itself authenticated and encrypted by iroh (QUIC/TLS). What you lose is the node's own sshd host-key
  identity, which gangli replaces with the certificate check.

The open console runs an SSH server (russh) on `127.0.0.1:2222` (configurable). Any SSH client — PuTTY,
OpenSSH, WinSCP, VS Code Remote, an LLM's `ssh` — connects there. The host is always `127.0.0.1`, so **the
username says where to go**:

| Username | Means |
|---|---|
| `root@nas` | user `root` on machine `nas` |
| `root+nas` | the same, for tools that refuse `@` in a username |
| `nas` | the machine's default user (the `desktop` user if one is logged in, else the service account) |
| `root@nas.home` | when two networks have a machine called `nas` |
| `desktop@nonna-pc` | **placeholder**: whoever is using the machine's desktop now (Windows: the active console session, else any logged-in session; macOS: the console user; Linux: the active graphical session). No password needed on the machine: the node runs as SYSTEM / root and starts the session with that user's token / uid |
| `system@nas` | the account the node service runs as (SYSTEM / root) |

The machine name is the one the console shows (from the hostname, renameable in the console); an id prefix
works too.

**Password**: rules in `gangli.console`, the most specific match wins — (machine, user) > (machine, any user) >
(any machine, user) > any. Each rule's password is generated when the rule is made and can be changed. One
rule for everything exists from the first start; OpenSSH users can list public keys instead.

```toml
[ssh]
listen = "127.0.0.1:2222"

[[ssh.login]]                 # every machine, every user
hosts = ["*"]
users = ["*"]
password = "q7vd-k2mx-9wfa"    # generated; change it if you like

[[ssh.login]]                 # grandma's PC, only her desktop session
hosts = ["nonna-pc"]
users = ["desktop"]
password = "nonna"
keys = ["ssh-ed25519 AAAA… michele@laptop"]
```

CLI: `gangli ssh login list|add|set|remove`, `gangli ssh password <machine> [user]` (prints the password
that applies). GUI: a table of rules, a "copy password" button on each machine.

**Keys instead of passwords**: a public key in the rule for `*` machines and `*` users lets every SSH client
that holds the private key in, with no password: PuTTY and WinSCP launched from the GUI use Pageant's keys
(PuTTY tries the agent's keys before the `-pw` password), OpenSSH uses its agent or `-i`; an AI agent adds its
own key through the API (`ssh_login_add`).

After the login the gateway opens a gangli stream to the node (there is **no SSH server on the nodes**):
shell or command with a PTY (ConPTY on Windows, `openpty` via rustix on Linux / macOS) as the requested user,
SFTP, and `direct-tcpip` / dynamic forwarding (`ssh -L`, `ssh -D`) into the machine's LAN.

### Launchers (the per-machine drop-down)

External programs, parametrized, started by the GUI drop-down on a machine or by `gangli open <machine>
--with <launcher> [--user <user>]`:

```toml
[[launcher]]
name = "PuTTY"
command = "putty -ssh {login}@127.0.0.1 -P {port} -pw {password}"

[[launcher]]
name = "Terminal (OpenSSH)"
command = "wt ssh -p {port} {login}@127.0.0.1"

[[launcher]]
name = "WinSCP"
command = "winscp sftp://{login_url}:{password_url}@127.0.0.1:{port}/"
```

Placeholders: `{node}`, `{network}`, `{user}`, `{login}` (= `user@node`), `{login_url}` / `{password_url}`
(URL-encoded), `{password}`, `{port}`. A first set for the current OS is written on first start.

### Integrated terminal

The GUI's terminal ("Terminal here", section 12) uses the same node-side session protocol directly (no SSH),
resumable, and attaches to the persistent terminals of section 6 as well. Implemented 2026-11-20.

**Images in the terminal** (Kitty graphics protocol, sixel, iTerm2 inline images) are escape sequences in the
terminal stream: the gateway and the node's PTY carry them untouched, so they work from Linux / macOS nodes when
the local terminal supports them (kitty, WezTerm, Ghostty, iTerm2; Windows Terminal: sixel; not PuTTY).
From Windows nodes ConPTY re-renders the output and most likely drops them **[verify]**. The persistent
terminal keeps a screen state with **alacritty_terminal** **[decided 2026-11-20: pure Rust, simpler build;
images are not a goal]**, which does not keep images: they work through the SSH gateway, not in the
persistent terminal.

### Saved tunnels

```toml
[[tunnel]]
name = "router"
node = "nas"                  # machine name (or name.network)
listen = "127.0.0.1:8080"     # on this computer
target = "192.168.1.1:80"     # as seen from the machine: its LAN, or localhost
enabled = true
```

Enabled tunnels listen whenever a console is open (GUI / tray / `console run`) and reconnect by themselves:
every new local connection dials the machine again if its session dropped (with resumable streams, phase 4,
open connections survive drops too). CLI: `gangli tunnel add|remove|list|on|off`; GUI: a list with switches.

### Which users a node can act as **[decided]**

- Service as SYSTEM / root (default): any local user, and the `desktop` placeholder.
- `gangli install --user NAME [--password …]` runs the service as that account instead: sessions only as
  that user. (Windows needs the account's password for the service manager.)
- Node run by hand (`gangli node`): only the user who runs it.

## 8. Their LAN

Always on **[decided]**: every node is a hop into its LAN.

- Saved tunnels (`gangli tunnel add`, section 7) for fixed targets; for any target, the SSH client's dynamic
  forwarding through the gateway: `ssh -D 1080 -p 2222 root+nas@127.0.0.1` (PuTTY: a "Dynamic" tunnel) makes
  a SOCKS proxy on this computer that exits from `nas`'s LAN. **[decided 2026-11-20]** no SOCKS server of
  our own for now; if needed later: one proxy where the host name picks the exit machine
  (`192.168.1.1.nas.gangli`, `nas.gangli:8080`, with `socks5h`).
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
  same `gangli.node`, so the node keeps its identity: SCM service (Windows, LocalSystem), systemd unit
  (Linux), LaunchDaemon (macOS). **[decided 2026-11-20]** Choosing the folder and its permissions is the
  job of whoever installs: the service runs as SYSTEM / root, so a folder ordinary users can write to lets
  them swap the binary (a folder of its own such as `C:\gangli`, `/opt/gangli`, writable by administrators
  only, is the sensible choice). gangli checks nothing and changes no folder permission; it only makes
  `gangli.node` (the node key) readable by the service alone (SYSTEM + Administrators, root, or the
  `--user` account) and gives it back on `uninstall`. `--user NAME` runs the service as that account
  (Windows also `--password`). `uninstall` keeps the identity (delete the folder to remove everything).
  The GUI's "Install as a service" runs the same `install`.
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
- **Joining from a phone** (`pair.rs`, `qr.rs`, `web/index.html`) **[decided 2026-11-22]**: no code to type,
  no camera on the machine, no port anywhere.
  - The **gangli wallet** is a static web page (GitHub Pages of this repository,
    https://mefistofelix.github.io/gangli/) that keeps, in the phone's browser storage, **public codes only**
    (several networks). A console's *Codes* dialog (and `gangli network codes NAME --qr`) shows a QR with the
    wallet's address and `#net=<public code>&name=…`: scanned with the phone's camera, the wallet opens and
    keeps that network. Private codes are never put in a QR, and the wallet refuses them.
  - On the machine: *This computer → Pair with your phone* (or `gangli pair`) shows a QR with the wallet's
    address and `#pair=<random 32-byte seed>`. Scanned with the phone's camera, the wallet opens; you pick a
    network. The wallet encrypts that network's public code with AES-256-GCM under
    `SHA-256("gangli-pair-v1" | seed)` and publishes `nonce | ciphertext` on the **pkarr relays** (TXT `_gangli`,
    the directory's format), signed by the ed25519 key whose seed is the seed. The machine polls that key
    **only while its QR is on screen** (every 3 s, at most 10 minutes), decrypts, checks that it is a public
    code, and joins; a console of the network then admits it as for any join.
  - Why it is safe: the relays see a random key and ciphertext; only who saw the machine's screen holds the
    seed, so nobody else can send it a network; only a public code ever travels (the power to *ask* to join).
    Why this way: a browser cannot speak iroh, and anything it can speak to a machine directly (HTTP,
    WebSocket) would need a port opened on it; a mailbox on public relays both reach outward needs none, and
    pkarr is already gangli's rendezvous (its relays allow browsers: CORS). The wallet has no dependency
    (WebCrypto: Ed25519, AES-GCM, SHA-256), and its Content-Security-Policy lets it talk only to the two pkarr
    relays. Fixed test vectors are shared by `pair.rs` and the wallet (`#selftest`).
  - `GANGLI_WALLET_URL` points the QR codes at another copy of the page (tests: a local server).

## 11. Updates (`update.rs`)

- **No release signature** **[decided 2026-11-20]**: only the consoles of the network (the owners) can send
  an update, and they can already run anything on the machine, so a signature on top would add a key to
  keep and nothing more. Trust in the binary itself is the OS's business (SmartScreen / Gatekeeper ask the
  first time) and the user's.
- `gangli update [machines…] --from <folder>` (a folder with a release, e.g. `gh release download build-N`):
  the console picks each machine's binary by platform and architecture (from its inventory) and sends it
  over the session. The node checks that it is an executable for its own platform and architecture (PE,
  ELF with the right machine, Mach-O), renames the running binary to `gangli….old`, puts the new one in place
  and restarts: the service manager starts it again (systemd `Restart=always`, launchd `KeepAlive`, the
  Windows recovery action set by `install`), a node run by hand starts the new binary itself. The `.old` file
  is removed at the next start.
- Not yet: automatic rollback if the new version does not come back.

## 12. GUI, tray, local API, LLM agents

- **Native GUI** (egui) **[decided: native, not browser]** (`src/gui/`), one window, unified, in English
  **[decided 2026-11-21]**. `gangli gui`, or a double click (Windows:
  the console window made for the double click is released; Linux / macOS: started without a terminal):
  - *Machines*: live list (online dot, system, LAN address, link direct / relay with RTT, since / last seen),
    filter; a machine's panel: a **terminal inside the window** ("Terminal here"), or each launcher (PuTTY,
    OpenSSH, WinSCP…), as a chosen user and optionally in a persistent terminal; copy the gateway password or an `ssh` command line, its persistent
    terminals (open / close), its tunnels (add), inventory, update from a release folder, forget;
  - *Networks*: create (dialog with the network's QR for the phone wallet, the public code, and the private
    one hidden until asked), codes, add one with a private code, remove; "prepare a gangli" writes a copy of this binary with the public code
    embedded next to it, for a relative;
  - *Tunnels*: list with on / off and state, add, remove;
  - *SSH*: gateway address, login rules (passwords hidden unless asked, copy), add / remove, launchers;
  - *API*: on / off, tokens (made here, shown once; revoke);
  - *This computer* (node): memberships and admission, **pair with your phone** (section 10), join with a
    public code (prefilled when embedded),
    run the node inside the window, install as a service (the same `gangli install`, from where it is), the
    installed service and uninstall; the files gangli keeps. A binary with an embedded code and no console
    network opens on this page.
- **Integrated terminal** (`gui/term.rs`, when the console lives in the window): tabs of shells or persistent
  terminals on machines, over the same resumable stream as the gateway (survives drops); alacritty_terminal
  keeps the screen, egui draws it (256 colours and truecolor, bold, underline, wide characters); xterm keys,
  bracketed paste, answers to the program's queries; drag selects and copies, right click or Ctrl+V pastes,
  the wheel scrolls back. Unix sessions get the system language (else `C.UTF-8`): without it shells drop
  accented input. **Predictive echo** on slow links (mosh-like, simplified): when the echo is late (> 40 ms)
  typed text shows at once, underlined, and echoed characters replace it as they come; predictions start
  on a line only after the machine has echoed something typed there, so a prompt without echo (password)
  never shows them; Enter, Esc or Ctrl stop them until the next echo. Verified with 250 ms of `tc netem`
  delay each way.
- The GUI **is** a console: it opens one in its process (owning the local port, the gateway, the tunnels).
  If a console is already open in the folder: a GUI there is brought up instead (`GuiShow` over the local
  API, the second process exits), a `gangli console run` is used through its local API.
- Backends: eframe (winit + OpenGL) and tray-icon on Windows and macOS. Linux (static binary, no system
  libraries): X11 in pure Rust (x11rb's own connection; XWayland on Wayland desktops), egui drawn by a CPU
  rasterizer into PutImage strips, keyboard from the server keymap (AltGr levels, reloaded on
  MappingNotify), CLIPBOARD both ways, cursor font, `Xft.dpi` scale; tray as a StatusNotifierItem over
  D-Bus (ksni). Without a tray host (bare X, WSLg) closing the window quits.
- Tests: `GANGLI_GUI_SCREENSHOT=file.png` (+ `GANGLI_GUI_SCREENSHOT_AFTER`, `GANGLI_GUI_PAGE`) saves the
  window as drawn and quits (both backends); `GANGLI_GUI_TEST_TRAY=1` closes the window at 2 s and opens it
  from the tray at 5 s, logging the window state (Windows: as the system sees it); `scripts/tray-test.py`
  is a StatusNotifier watcher + host that prints gangli's tray and clicks its menu; `scripts/gui-drive.sh`
  drives the Linux GUI with xdotool; `scripts/tray-panel.sh` runs a real tray (xfce4-panel 4.18) in WSLg: the icon
  shows, a left click opens the window, the panel's menu opens terminals and quits. Verified 2026-11-20: Linux in WSLg (clicks, typing with accents / `@`
  / `€`, copy and paste both ways, tray menu over D-Bus, close to tray and back, second instance, GUI on a
  `console run`), Windows (pages, close to tray and back), macOS on the GitHub runner (window drawn, screenshot).
- **Link quality in the GUI**: every machine shows whether its traffic goes **direct** (hole-punching worked)
  or through a **relay** (slower, rate limited on the public relays), with the RTT; the CLI has the same in
  the `LINK` column.
- **Tray icon** while open (closing the window keeps it in the tray): online machines count, open, quit.
  `tray-icon` on Windows / macOS, `ksni` (pure Rust D-Bus) on Linux: nothing linked to GTK, so the Linux
  binary stays a single file. **Start at login** is an option the user turns on in *This computer*
  (`gui/autostart.rs`): a login item of the user (Windows `Run` key, XDG autostart entry, LaunchAgent
  without KeepAlive) that runs `gangli gui --hidden`, which stays in the tray (or shows the window after
  3 s when there is no tray). Never a service.
- **Local API** **[decided]**: off by default, enabled from the GUI or `gangli api enable` (on the console's local port,
  127.0.0.1:7373 by default, `[console] port`), served while a console is open. Named tokens made and revoked from the GUI or `gangli api token add|remove`,
  never through the API itself; scopes `read` (status, machines, details) < `exec` (also commands, jobs,
  terminals) < `admin`: **everything the CLI and the GUI do except the API and its tokens** **[decided
  2026-11-21]**, so an AI agent with an admin token can list and identify the machines, run commands, add its
  own key to the SSH gateway and connect with any client, make tunnels. Routes: `POST /v1/op` with an
  operation as JSON (`{"op":"nodes"}`), `GET /v1/status`, `GET /v1/nodes`, `GET /v1/nodes/<name>`, `GET
  /v1/help` (every operation with its parameters and scope, for agents); `Authorization: Bearer <token>`.
  Making a token gives ready instructions for an agent (token, address, the main calls): the GUI copies them,
  `gangli api token add NAME --scope admin --save FILE` writes them to an owner-only file. See
  [API.md](API.md).
- The CLI and GUI talk to the open console through the same local HTTP with the console's own token (in
  the owner-only `gangli.console`; the open console owns the port, which is also the single-instance guard), so an LLM using the CLI needs no token at all.
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
  normally. Verified in WSLg (section 12); the binary stays fully static (`static-pie linked`).
- No local builds: everything is built by the repository's GitHub Actions (public repository) and checked with
  their binaries; the workflow also runs an end-to-end test on each runner (service install, exec as SYSTEM /
  root, GUI screenshot, update, uninstall). Verified 2026-11-21 on the three OSes.

## 14. Libraries (researched 2026-11)

| Need | Choice | Notes |
|---|---|---|
| P2P transport | `iroh` 1.5 | multipath, self-hostable relay; `pigeons` (same authors) is the reference for "ssh over iroh" |
| SSH server / SFTP | `russh`, `russh-sftp` | maintained (0.6x); pick a version without RUSTSEC-2026-0007 |
| VT engine | `alacritty_terminal` **[decided]** | pure Rust, used by Alacritty and Zed; libghostty-vt (more complete, Kitty graphics) set aside: needs Zig and has MSVC link quirks, images are not a goal |
| PTY | `rustix` openpty + ConPTY via `windows-sys` | `portable-pty` (the usual wrapper) has had no release since Feb 2025 |
| Session persistence design | study `shpool` (+ `shpool-vterm`), `osewa`, `RoSE` | shpool: detached shells + screen restore; osewa: QUIC/WS racing, predictive echo; RoSE: mosh-like over quinn |
| Inventory | `sysinfo` 0.40 | |
| Directory | `pkarr` 9 (Mainline DHT + HTTP relays), `chacha20poly1305` | 1.5M downloads; RustCrypto AEAD, 120M downloads |
| Tray | `tray-icon` (Windows, macOS), `ksni` (Linux) | no GTK linking |
| Service | `windows-service`, launchd plist, systemd unit | |
| GUI | `egui` 0.37; `eframe` on Windows / macOS; `x11rb` + own CPU renderer on Linux | no GL or X11 library linked on Linux |
| Async | `tokio` | |
| macOS re-sign | `apple-codesign` | for `pack` |

Code from osewa / pigeons can be read and adapted (Apache-2.0 / MIT, keep notices); no external binaries are
called for core features.

## 15. Known limits (now)

- The private code cannot be revoked: a leak means a new network.
- The public code is fixed per network (no expiry or use count).
- A machine can be node and console at the same time (separate keys); the console does not list its own
  node specially.

## 16. Roadmap

1. Skeleton, builds on GitHub Actions, release pipeline. ✓
2. Service install / uninstall / elevation on the three OSes. ✓
3. iroh, networks as keys, public / private codes, directory on pkarr, certificates, live sessions,
   inventory push, node without install, embedded code, one-shot or open console. ✓
4. SSH gateway, sessions as users (Windows tokens, Unix uid), SFTP, saved tunnels, launchers. ✓
5. Resumable streams; `exec` and jobs; own relays; directory pages; self-update; persistent terminals
   with state sync. ✓
6. GUI with tray (node + console unified), API tokens in the GUI. ✓
7. Integrated terminal in the GUI, with predictive echo. ✓ Later: public code rotation, update rollback.
