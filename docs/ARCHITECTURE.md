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

One unified GUI (section 12): *this computer* can be a node (join with a public code, the phone, or the networks the binary carries, start,
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
- **Extra relays per network, set on a console, sent to the machines [decided 2026-11-22]**. The n0 relays
  stay the default and are always kept; a network may add its own (`relays` in its `[[network]]` of
  `gangli.console`; GUI *Networks → Relays*, `gangli network relays NAME URL…`, API `network_relays`).
  - Relays know nothing of networks: they forward end-to-end encrypted packets between endpoint ids, and
    gangli's checks (certificates, join secret) happen between console and node. Each endpoint has a list,
    picks the closest as its home relay and publishes it in its address record; whoever dials it goes to that
    relay even when it is not in its own list (iroh 1.5 connects to any relay URL a peer's address names; its
    own list only chooses the home relay and carries an optional access token). So a console on the n0 relays
    reaches a node homed on ours, and the list does **not** belong in the network codes: codes are printed,
    embedded and kept in the phone wallet, and would all go stale when a relay changes.
  - The console sends the list to the network's machines over the authenticated session (`Hello`, then the
    `Consoles` frame when it changes); each node keeps it per
    membership in `gangli.node` and uses the union of its networks' lists, added at runtime
    (`Endpoint::insert_relay`) on top of the n0 ones: a console can add relays, never take the n0 ones away.
  - Consoles of a network: the most recent setting wins (`relays_at`), carried by `Sync`. A console uses the
    relays of all its networks.
  - Each node reports its home relay in its inventory (machine details: *Its relay*); the console shows its
    own (`status` → `relay`).
  - Local setting as well, per file: `relays = […]` in `[node]` / `[console]`, `relays_n0_fallback = true`
    (default; `false` drops the n0 ones on that machine only), applied when the node / console starts.
  - Our relay is open to anyone, like n0's **[decided 2026-11-22]**. Closing it would need an access token on
    every machine that talks through it (dialers included), sent by the consoles the same way.
- **Direct links, also without UPnP [decided 2026-11-22]**. Hole punching fails when one side is behind a
  NAT that gives a port per destination (mobile networks, CGNAT: the aunt's 4G) and the other side's router
  does not open a port. A direct path then needs one side reachable from outside, which gangli tries in this
  order: IPv6 on both sides (iroh uses it by itself); a UDP port the console's router forwards by hand
  (a port range forwarded to the PC, a DMZ); the router's UPnP / NAT-PMP / PCP (iroh's portmapper).
  - The open console finds a forwarded port by itself (`probe_ports` in console.rs), 90 s after starting,
    once per public IPv4 address and while none has been found (forwards stay when the address changes),
    or when asked (*This computer → Network → Look for a forwarded port*, `gangli net --probe`, API
    `net_probe`). A machine outside (one whose inventory shows another public address, else one reached
    through a relay) sends a datagram with a random token to ports of the console's public IPv4
    (`Req::ProbeUdp`); the console listens on those ports and the first where a token arrives is the one.
    Plan (`proto::probe_plan`): at most 4096 ports a request, about 800 datagrams a second; first the usual
    ports (80, 443, 53, 500, 4500, 3478, 51820, …) with one port every 8 below 4096 and one every 256
    above, then every port below 4096 (hand-made ranges like `0-3000`), then one every 32 and one every 8
    above: any forwarded range of 8 ports or more is found, ≈ 12,000 datagrams at most, never a full sweep.
    Ports the console cannot listen on (in use; below 1024 without the right on Linux) are skipped.
  - The port found goes in `gangli.console` (`udp_port`, `udp_port_found`, `udp_probed`) and the endpoint
    binds it from the next start (`util::bind_endpoint`, a random port if it is taken); the GUI offers the
    restart. `udp_port` set by hand is never probed over; nodes have `udp_port` in `[node]` too.
  - Diagnostics: the node reports how it reaches the Internet in its inventory (`net`: UDP v4/v6, public
    addresses, NAT kind from iroh's net report; machine details *Its network*), and every path of a link
    (*Paths*); the console's own in `status` → `net` (*This computer → Network*, `gangli net`).
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

**A tab outlives its session [decided 2026-11-22]**: when the stream cannot be resumed (the machine restarted,
or stayed away past what the node keeps), the tab says so and waits for the machine, then goes on in the
same tab with the screen kept: a persistent terminal is reattached by name, otherwise a new shell starts,
with a dim line saying which. After the shell ends, Enter starts a new one there; a refusal (unknown user)
waits for Enter to try again. Keys typed while the machine is away are dropped, not replayed into the next
shell. Each tab shows its link: a dot (green direct, amber relay or reconnecting, grey ended, red failed),
and under the tabs the active one's path, RTT and its own traffic per second; hovering a tab shows the
details and the traffic of the whole connection with that machine (`conn.stats()` bytes, sampled each
second).

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
version, online since / last seen. Crate `sysinfo`, local users from `/etc/passwd` on Linux. Also who is
logged in now (`proc::logged_in`: Windows sessions, the macOS console user, Linux login sessions), plugged
in or on battery and the charge (`power::source`; the charge counts for a push only by fifths), and how it
reaches the Internet (section 4).

**Power [decided 2026-11-22]**: a managed machine that sleeps or hibernates is offline until someone wakes
it (seen on the aunt's laptop: S3 sleep). The owner reads and sets a machine's sleep and hibernation
timeouts, plugged in and on battery, from a console (machine details → *Sleep settings…*, `gangli power NODE
--sleep-ac 0`, API `power`; `Req::Power`). The screen's timeout is shown and never changed: the screen still
turns off. Windows: the active power plan through the power API (powrprof, no external program); macOS:
`pmset` (sleep only, hibernation follows it); Linux: sleep is on or off for the whole system (systemd's
sleep targets masked), the idle timeouts belong to the desktop. Changing them needs the service.

Names: the admitting console names nodes (slug of the hostname and, when someone is logged in, that user:
`asuspc-rocar` **[decided 2026-11-22]**; unique per network, written in their certificate); addressing is `name` or
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
- **Networks carried by the binary [decided 2026-11-22]**, without recompiling: a trailer at the end of the
  file (`JSON | u32 length | "GANGLI-NETS1"`; the JSON lists one or more networks: public code, name, extra
  relays), or a `gangli.networks` file with the same JSON next to the binary. Only public codes (refused
  otherwise, when packing and when reading). Made by *Networks → Create gangli executable…* (tick the
  networks; a copy of this binary next to it) or `gangli pack --network NAME… [--code CODE…] [--from
  <release binary of another OS>] --out <file>`. **Used once [decided 2026-11-22]**: the first time it
  starts (any command, the window too) where there is no `gangli.node` yet, `gangli.node` is made with those
  networks and their relays, no question asked; then it is a normal gangli (the window runs the node since
  there are networks; networks left stay left). Deleting `gangli.node` makes the next start put them back.
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
    address and `#pair=<random 32-byte seed>&name=<machine>`. Scanned with the phone's camera, the wallet
    opens on "Add <machine> to networks": you tick one or more networks and tap *Add*. The wallet encrypts them
    with AES-256-GCM under `SHA-256("gangli-pair-v1" | seed)` and publishes `nonce | ciphertext` on the **pkarr
    relays** (TXT `_gangli` in parts `a…`, `b…`, the directory's format), signed by the ed25519 key whose seed
    is the seed. Plaintext: `0x02`, then per network its 49 code bytes, a name length and the phone's name
    for it (≤ 24 bytes); at most 6 networks (a pkarr packet holds 1000 bytes: 6 with long names make 885). The machine polls that key **only while its QR is
    on screen** (every 3 s, at most 10 minutes), decrypts, checks that every code is a public one, and **asks
    the person at the machine to confirm** (GUI dialog with the networks ticked, or `[Y/n]` in a terminal;
    `gangli pair --yes` for scripts). Confirmed, it joins and its node runs (section 12); a console of each
    network admits it as for any join.
  - Wallet flow **[decided 2026-11-22]**: scanning a network QR saves it at once (a public code: no question,
    *Undo* and *remove* are there); the phone never asks to confirm a machine, the machine does. A machine
    scanned while the phone has no network yet is remembered for 10 minutes: saving a network then offers to
    add that machine without scanning it again.
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
- **One convention for every OS and every way gangli runs [decided 2026-11-22]** (service, node by hand,
  window, `console run`; as SYSTEM / root or as a user): **the new binary goes next to the running one as
  `<binary>.update`** (`gangli.update`, `gangli.exe.update`). It gets there by hand, by `gangli self-update
  --from <binary|release folder>` / the API's `self_update` (this computer's gangli), or over a session when
  a console updates a machine (`gangli update [machines…] --from <release folder>`, the console picking each
  machine's binary from its inventory). Then:
  1. *Trial*: a first look (PE, ELF with the right machine, Mach-O), then the candidate is run: `version
     --json` must answer this OS and architecture within 20 s (a damaged file, the wrong one, an unsigned
     macOS binary fail here). A failed one is set aside as `<binary>.failed`; nothing changes.
  2. *Swap*: the running binary becomes `<binary>.rollback` (a running file can be renamed everywhere, not
     overwritten), the candidate takes its name, dated now, and the process restarts on it: a service exits
     and its manager starts it again (systemd `Restart=always`, launchd `KeepAlive`, the Windows recovery
     action set by `install`); anything else starts the new binary with the same arguments (the window as
     `gangli gui`, without a console window), which first waits for the old process to be gone
     (`GANGLI_WAIT_PID`: the window's single instance, the node's status port).
  3. *Probation*: the update is kept once a long-running gangli on it has run 30 s or ended normally
     (`.rollback` → `<binary>.previous`, the last version, for a rollback by hand). If three minutes after
     the swap none has (the new binary keeps failing; a service is restarted every 5 s), the next start puts
     `.rollback` back and the new binary becomes `.failed`.
  4. *Watch*: each long-running gangli looks every 30 s, on the local disk only (no network: not polling),
     for a `.update` to apply, or for its binary replaced by another gangli of the same folder (the window
     updated while the service runs from the same file), and restarts on the new one.
  A folder the running user cannot write (a service installed in `/opt`, updated from a user's window)
  stops it with a clear error; the binary stays as it was.

## 12. GUI, tray, local API, LLM agents

- **Native GUI** (egui) **[decided: native, not browser]** (`src/gui/`), one window, unified, in English
  **[decided 2026-11-21]**. `gangli gui`, or a double click (Windows:
  the console window made for the double click is released; Linux / macOS: started without a terminal):
  - *Machines*: live list (online dot, system, LAN address, link direct / relay with RTT, traffic received /
    sent per second, since / last seen),
    filter; a machine's panel: a **terminal inside the window** ("Terminal here"), or each launcher (PuTTY,
    OpenSSH, WinSCP…), as a chosen user and optionally in a persistent terminal; copy the gateway password or an `ssh` command line, its persistent
    terminals (open / close), its tunnels (add), inventory, update from a release folder, forget;
  - *Networks*: the networks, each with codes, relays (extra ones, sent to its machines; the console's own
    relay shown) and remove; one *New network…* button at the top opens a dialog: create one (then the
    dialog with its QR for the phone wallet, the public code, and the private one hidden until asked) or add
    one already owned with its private code; *Create gangli executable…* (next to it) writes a copy of this
    binary carrying the ticked networks, for a relative;
  - **Every page puts its actions above its list [decided 2026-11-22]**: the main one ("New …") at the top
    right, opening a dialog; each row has its own (*Edit*, *Remove*…).
  - *Tunnels*: *New tunnel…*; list with on / off and state, *Edit* (machine, address, local port), *Remove*;
  - *SSH*: *New rule…*; gateway address, login rules (passwords hidden unless asked, copy), each with
    *Edit* (machines, users, password, keys one per line: `ssh_login_set`) and *Remove*; launchers;
  - *API*: *New token…* (name, scope); the tokens, each with *View / edit* (its value and the instructions
    for an AI agent to copy, its name and scope to change: `ApiTokenSet`), *Copy token*, *Copy
    instructions*, *Revoke*, at any time (`gangli api token show NAME [--save FILE]`); on / off;
  - *This computer* (node): **a summary of its networks [decided 2026-11-22]**: for each, admitted or not
    (and why not), its name there, the consoles connected now (and since when) or when the last one left,
    and *Leave*; **pair with your phone** (section 10), join with a public code (prefilled when embedded),
    and **one choice for how the node runs [decided 2026-11-22]**:
    *Off*, *While gangli is open* (in the window, as this user) or *Always, as a service* (`gangli install`,
    from where it is). Changing it does the rest: stop the window's node, install or uninstall the service
    (one system prompt), start the window's node. Without the service, the window runs the node by itself
    whenever it has a network (`[node] window_node`, on unless *Off* was chosen): joining a network (phone or
    code) starts it, nothing to press. With the service, joining runs the installed binary's `gangli join`
    (one prompt for all the networks). The files gangli keeps. A binary with an embedded code and no console
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
  A GUI already open on this computer is brought up instead (below); a `gangli console run` open in the
  folder is used through its local API.
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
- **The node's status, one node per computer [decided 2026-11-22]**: the running node (window, terminal or
  service) answers `GET /node/status` on `127.0.0.1:7374` (`[node] status_port`; none under
  `GANGLI_DATA_DIR` unless set): its memberships, the consoles connected now, its relay. Read-only and
  without secrets, only with the `X-Gangli` header; it is how the GUI and `gangli status` see a service
  whose `gangli.node` only the system can read. The port also keeps a second node from starting on the same
  computer (it retries every 10 s, so it takes over when the first one stops).
- **Leaving a network** (`gangli leave`, *Leave* in the GUI): the membership goes from `gangli.node`; the
  running node then sends `Leaving` on that network's live sessions and closes them; those consoles forget
  the machine (its id goes to the removed list, which `Sync` passes to the network's other consoles).
  Joining again with the public code takes it off the list.
- **One window per computer [decided 2026-11-22]**: a gangli started while another one's window is open (from
  any folder) brings that window up and closes. It asks with `POST /gui/show` on its folder's console port
  and on the default one (7373): no token, since it can only raise the window (or learn there is none), and
  only with an `X-Gangli` header, which a web page cannot send to localhost without a CORS preflight this
  server never answers. On Windows it first allows the other process to take the foreground, so the window
  really comes up instead of blinking in the taskbar.
- **Window size [decided 2026-11-22]**: the last size and place, saved in `gangli.console` (`[gui] window =
  [x, y, w, h]` in points, once the window has been still for a second; not while minimized, maximized or
  full screen). The first time: about three quarters of the screen in points (800–1280 × 560–860, never more
  than fits), centered; a 1920×1080 screen at 150% is 1280×720 points, where a fixed 1080×700 was nearly the
  whole screen. On X11 the place uses StaticGravity, so it does not drift by the title bar at each start.
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
