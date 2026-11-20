# AGENTS.md

Guide for anyone (human or AI agent) working on gangli. Talk to the owner in Italian; code, comments, docs
and commit messages in English.

## What this is

gangli is a remote management tool for the owner's machines and those of family and friends: one binary for
Windows, Linux and macOS with two roles (**node**: a managed machine, system service; **console**: the
owner's side, a user process), peer-to-peer over iroh, no open ports, no server, resilient to disconnects and
slow networks. The design and the decisions taken with the owner are
in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): read it before changing anything structural, and update it
when a decision changes.

## Rules (keep them)

- **One binary.** No helper executables, no calling external programs for core features (iroh, ssh, PTY,
  terminal are libraries). Launching the user's own tools (PuTTY, ssh) from the console is fine; so is calling the
  OS service managers (`systemctl`, `launchctl`, `icacls`) and elevation tools (`osascript`, `pkexec`).
- **No inbound ports, ever.** Every listener is either iroh (peer-authenticated) or bound to `127.0.0.1` /
  local IPC.
- **Authenticate before acting.** The peer's endpoint id is authenticated by iroh's handshake; every request
  must then prove its right before anything is done: a certificate signed by the network key that names that
  endpoint id (console certificate towards a node, node certificate towards a console), or a valid invite /
  join secret (public code) towards a console. Anything else gets an error answer and nothing more.
- **A network is its key.** The private code (the key) makes a console an owner; the public code and an
  embedded binary must never grant more than "become a node". Only a public code may be embedded or put on a
  managed machine.
- **Nodes never talk to other nodes**, only to consoles with a valid certificate.
- **The console is never a service or a background daemon**: it lives in the GUI / tray, in `console run`,
  or in a one-shot CLI process. The node may run by hand or as a service.
- **No polling of nodes.** Presence comes from live sessions; a node sends traffic only for pings on an open
  session, knocks on events (start, address change, session drop) and inventory changes. A periodic network
  call from the node is a regression.
- **Portable, two files.** Everything gangli keeps goes in `gangli.node` and `gangli.console` (TOML) next to
  the executable, plus `gangli.log` (`paths.rs`); never a system or user folder, never `%APPDATA%`,
  `~/.config`, `/var/lib`, no lock, port or temp files next to them either. Only the OS service registration is
  written elsewhere, by `install`.
- **Secrets** (node keys, console keys, network keys, API tokens, `.tools/box-password`) live in files readable
  only by their owner (root / SYSTEM / the user) and are never printed, logged or committed.
- **No consent UI on managed machines** (owner's decision), but nothing hidden either: normal service name,
  normal path, working `uninstall`.
- **Reconnection is invisible**: a change that makes a dropped iroh connection visible to a local TCP client
  (PuTTY, ssh, a forward) is a regression.
- **LLM friendly**: every CLI command supports `--json`, has stable exit codes and never prompts when stdin
  is not a terminal; the API never takes passwords, only tokens approved in the GUI.
- Third-party code adapted from osewa / pigeons / iroh examples keeps its license notice (Apache-2.0 / MIT).

## Layout

```
Cargo.toml            single crate for now (bin `gangli`); split into a workspace when it grows
src/main.rs           CLI (clap) and dispatch
src/node.rs           node body (by hand or service): serves console sessions, knocks, pushes inventory
src/node_cmd.rs       node-side commands (join, leave, status) on the service state
src/console.rs        console (open or one-shot): live sessions, admission, sync, directory, IPC
src/console_cmd.rs    console-side commands (network, nodes, info, forget, console)
src/ipc.rs            CLI ↔ open console (local HTTP + internal token), else a one-shot console
src/http.rs           minimal HTTP/1.1 of the local API (no dependency)
src/ssh.rs            console SSH gateway (russh): user+machine logins, rules, launchers, forwards
src/proc.rs           node: shell / command with ConPTY or /dev/ptmx, as system / desktop / a user
src/sftp.rs           SFTP v3 server, run by the node as `gangli sftp-server` in the user's process
src/resume.rs         resumable streams (sessions, forwards survive drops)
src/term.rs           persistent terminals (alacritty_terminal screen, redraw, state sync)
src/update.rs         release key, signatures, verified self-update
src/directory.rs      the network directory on pkarr (DHT + relays), node ids encrypted
src/embed.rs          public code embedded in a binary (`pack`) or in gangli.code next to it
src/proto.rs          wire protocol gangli/2: messages, frames, connect/request helpers
src/cert.rs code.rs   certificates; public / private network codes and derived secrets
src/state.rs store.rs node state (system or user folder); locked owner-only JSON files
src/inventory.rs      what a node reports (sysinfo)
src/elevate.rs        native elevation (UAC with output relay, osascript, pkexec/sudo)
src/service/          system service per OS (SCM, launchd, systemd)
docs/ARCHITECTURE.md  design and decisions
docs/RELAY.md         own iroh relay options for later (free VM, Cloudflare Tunnel, Workers)
scripts/remote-build.sh  builds the three OSes on build-box and publishes a release
scripts/box-build.sh     what runs on the build machine (one target OS per run)
.tools/               git-ignored: build-box clone, box-password, release signing key
```

## Building

There is no local Rust toolchain on the owner's Windows PC; builds happen on the owner's **public blind build
machine `mefistofelix/build-box`** (GitHub Actions, free on public repositories; its README.md and AGENTS.md
explain it). From the project root in Git Bash:

```bash
bash scripts/remote-build.sh            # build linux + windows + macos, publish release build-N
bash scripts/remote-build.sh --no-release   # build only, results in out/box-<os>/
```

- The working tree (tracked and new files, not the ignored ones) is staged with a `build.sh` calling
  `scripts/box-build.sh`, sent encrypted with the password in `.tools/box-password` (the secret `BOX_PASSWORD`
  of build-box; same file as the other projects, never print it). Three runs go in parallel: `ubuntu-latest`
  (linux x86_64 + aarch64, static musl via cargo-zigbuild), `windows-latest` (x86_64 msvc, static CRT),
  `macos-latest` (universal, ad-hoc signed).
- Toolchains (rustup, zig, cargo-zigbuild) and the target dir are kept in `$CACHE`, per OS.
- Logs come back in `out/box-<os>/build.log`.
- Release key: `.tools/release.key` (secret, git-ignored, never print it) and `.tools/release.pub`; made once
  with `gangli release keygen`. With them the build bakes the public half in and signs every binary.
- Builds hit GitHub's API limit (5000 requests / hour; box.sh polls): batch changes, build rarely.
- Push HEAD before a release build (the script checks it). The release `build-N` (N = newest + 1, also the
  version `0.0.N` baked into the binary through `GANGLI_BUILD`) of this **private** repository gets
  `gangli-linux-x86_64`, `gangli-linux-aarch64`, `gangli-windows-x86_64.exe`, `gangli-macos-universal` and
  `SHA256SUMS`.

## Testing

- Unit tests run on build-box as part of every build (`cargo test` on the native target of each runner).
- End to end on one PC, no elevation, no service: `GANGLI_DATA_DIR` puts the files in another folder, one
  scratch folder per simulated machine (or simply copies of the binary in separate folders). `network create` on a
  console (`console run` in the background of the test), `node --code` for one or more nodes (each its own
  state dir), then `nodes` / `info`; kill a node and restart it to check that it shows offline and comes back
  by itself; a second console dir with `network add <private code>` must find the machines. The traffic
  goes through the real n0 relays, pkarr relays and the DHT. Close the console with `gangli console stop`.
- Network behaviour (drops, slow links) is tested with an in-process loss/latency shim on the resumable stream
  layer, not only by hand.
- Before a release that touches install / service code, try install → join → uninstall on all three OSes.
