# AGENTS.md

Guide for anyone (human or AI agent) working on gangli. Talk to the owner in Italian; code, comments, docs
(AGENTS.md, README), the GUI and commit messages in English.

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
  call from the node is a regression. The one exception: while a pairing QR is on screen (`gangli pair`), the
  machine polls the relays for the phone's answer (every 3 s, at most 10 minutes).
- **The phone wallet holds public codes only** and talks only to the pkarr relays; a private code never goes
  into a QR or a browser.
- **Portable, two files.** Everything gangli keeps goes in `gangli.node` and `gangli.console` (TOML) next to
  the executable, plus `gangli.log` (`paths.rs`); never a system or user folder, never `%APPDATA%`,
  `~/.config`, `/var/lib`, no lock, port or temp files next to them either. Only the OS service registration is
  written elsewhere, by `install`, and the login item (`Run` key, XDG autostart, LaunchAgent) when the user
  turns "start at login" on in the GUI.
- **Secrets** (node keys, console keys, network keys, API tokens) live in files readable
  only by their owner (root / SYSTEM / the user) and are never printed, logged or committed.
- **No consent UI on managed machines** (owner's decision), but nothing hidden either: normal service name,
  normal path, working `uninstall`.
- **Reconnection is invisible**: a change that makes a dropped iroh connection visible to a local TCP client
  (PuTTY, ssh, a forward) is a regression.
- **LLM friendly**: every CLI command supports `--json`, has stable exit codes and never prompts when stdin
  is not a terminal; the API never takes passwords, only tokens approved in the GUI.
- **No backward compatibility, ever** **[decided 2026-11-22]**: gangli is new and the owner's own; change a
  protocol, file format, QR or CLI outright and delete the old path (no old formats, aliases, migrations or
  "older gangli" fallbacks).
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
src/update.rs         self-update (platform check, swap, restart)
src/pair.rs           joining from a phone: pairing QR, encrypted answer through the pkarr relays
src/qr.rs             QR codes (matrix; terminal rendering)
src/directory.rs      the network directory on pkarr (DHT + relays), node ids encrypted
src/embed.rs          public code embedded in a binary (`pack`) or in gangli.code next to it
src/proto.rs          wire protocol gangli/2: messages, frames, connect/request helpers
src/cert.rs code.rs   certificates; public / private network codes and derived secrets
src/state.rs store.rs node state (system or user folder); locked owner-only JSON files
src/inventory.rs      what a node reports (sysinfo)
src/elevate.rs        native elevation (UAC with output relay, osascript, pkexec/sudo)
src/service/          system service per OS (SCM, launchd, systemd)
src/gui/              the window: app.rs (egui pages, same everywhere), client.rs (in-process console or the
                      open one), native.rs (eframe + tray-icon: Windows, macOS), x11.rs + raster.rs + sni.rs
                      (Linux: X11 protocol, CPU renderer, D-Bus tray), term.rs (integrated terminal), autostart.rs (login item), icon.rs, png.rs
web/index.html        the gangli wallet (GitHub Pages): public network codes on the phone, sends one to a machine
docs/ARCHITECTURE.md  design and decisions
docs/API.md           the local API for programs and AI agents
docs/RELAY.md         own iroh relay options for later (free VM, Cloudflare Tunnel, Workers)
.github/workflows/build.yml  the builds, tests and releases (GitHub Actions of this public repository)
.github/workflows/pages.yml  publishes web/ on GitHub Pages (the address in the pairing QR codes)
scripts/ci.sh            runs that workflow from here, waits, downloads the results into out/ci/<os>/
scripts/ci-build.sh      the build of one OS on its runner
scripts/ci-test.sh       end-to-end test on the runner (service, console, exec, GUI, update, uninstall)
scripts/gui-drive.sh     drives the Linux GUI with xdotool (WSLg) for tests
scripts/tray-test.py     a StatusNotifier watcher + host: prints gangli's Linux tray, clicks its menu
scripts/tray-panel.sh    a real tray in WSLg (xfce4-panel, notification area only) for GUI tests
scripts/relay/          our iroh relay VM: setup.sh (relay with its own certificate, firewall, keep-alive), knockd.py + knock.ps1 / knock.py (everything but the relay after a knock)
.tools/               git-ignored local tools
```

## Building

**No local builds** **[decided 2026-11-20]**: the repository is **public** and builds on its own **GitHub
Actions** (`.github/workflows/build.yml`, free on public repositories), started by hand, never on every push;
things are verified with the binaries it produces (`out/ci/<os>/`, or a release). From the project root in Git
Bash, after `git push`:

```bash
bash scripts/ci.sh                # build linux + windows + macos, unit tests, end-to-end test; results in out/ci/<os>/
bash scripts/ci.sh --no-test      # build and unit tests only
bash scripts/ci.sh --release      # and, if everything passed, publish the release build-N
```

(or Actions → build → Run workflow on GitHub.)

- Three jobs in parallel: `ubuntu-latest` (linux x86_64 + aarch64, static musl via cargo-zigbuild),
  `windows-latest` (x86_64 msvc, static CRT), `macos-latest` (universal, ad-hoc signed: Apple Silicon runs
  nothing without a signature). Toolchains (zig, cargo-zigbuild), the cargo registry and the target dir are
  cached in `~/.gangli-ci` per OS.
- Runner minutes, macOS above all: batch changes, run rarely, keep the end-to-end test short.
- The release `build-N` (N = newest + 1, also the version `0.0.N` baked into the binary through
  `GANGLI_BUILD`) gets `gangli-linux-x86_64`, `gangli-linux-aarch64`, `gangli-windows-x86_64.exe`,
  `gangli-macos-universal` and `SHA256SUMS`, and points at the commit that was built. A failed test means no
  release.
- The repository is public: nothing secret in it, ever (codes, keys, tokens, passwords of real machines).

## Testing

- Unit tests run on every build (`cargo test` on the native target of each runner).
- End to end on one PC, no elevation, no service: `GANGLI_DATA_DIR` puts the files in another folder, one
  scratch folder per simulated machine (or simply copies of the binary in separate folders). `network create` on a
  console (`console run` in the background of the test), `node --code` for one or more nodes (each its own
  state dir), then `nodes` / `info`; kill a node and restart it to check that it shows offline and comes back
  by itself; a second console dir with `network add <private code>` must find the machines. The traffic
  goes through the real n0 relays, pkarr relays and the DHT. Close the console with `gangli console stop`.
- Network behaviour (drops, slow links) is tested with an in-process loss/latency shim on the resumable stream
  layer, not only by hand.
- The end-to-end test of `scripts/ci.sh` (`scripts/ci-test.sh` on each runner): install as a service → online →
  exec as SYSTEM / root → GUI screenshot → update (the service restarts) → uninstall, through the real relays;
  `out/ci/<os>/test.log`, `out/ci/<os>/gui-<os>.png`. Needed before a release that touches install / service code.
- GUI, with CI binaries (Linux in WSLg, `DISPLAY=:0`; Windows directly): `GANGLI_GUI_SCREENSHOT=out/x.png GANGLI_GUI_PAGE=networks
  gangli gui` saves the window as drawn and quits (read the PNG to check it); `GANGLI_GUI_TEST_TRAY=1` checks
  close-to-tray and reopen; on Linux `scripts/tray-panel.sh start` runs a real tray (xfce4-panel) in WSLg, and
  `scripts/tray-test.py` is a scriptable stand-in; `scripts/gui-drive.sh`
  clicks and types with xdotool. In WSL use another console port than the Windows side (`[console] port`):
  WSL2 forwards localhost, so both cannot own 7373.
