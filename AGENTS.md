# AGENTS.md

Guide for anyone (human or AI agent) working on gangli. Talk to the owner in Italian; code, comments, docs
and commit messages in English.

## What this is

gangli is a remote management tool for the owner's machines and those of family and friends: one binary for
Windows, Linux and macOS with three roles (agent, master / control center, client), peer-to-peer over iroh, no
open ports, resilient to disconnects and slow networks. The design and the decisions taken with the owner are
in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): read it before changing anything structural, and update it
when a decision changes.

## Rules (keep them)

- **One binary.** No helper executables, no calling external programs for core features (iroh, ssh, PTY,
  terminal are libraries). Launching the user's own tools (PuTTY, ssh) from the client is fine.
- **No inbound ports, ever.** Every listener is either iroh (peer-authenticated) or bound to `127.0.0.1` /
  local IPC.
- **Authenticate before acting.** The peer's endpoint id is authenticated by iroh's handshake; every request
  must then prove its right before anything is done: a client certificate naming that endpoint id, signed by
  the network key (and, on the master, still in the paired-clients list), or on a master a valid join /
  pairing secret. Anything else gets an error answer and nothing more.
- **The master role is the network private key**, never a flag in a join code. Join codes and embedded
  binaries must not be able to grant more than "enroll a node".
- **Agents never accept connections from other agents**, only from clients with a valid certificate.
- **Secrets** (node keys, network keys, client keys, API tokens, `.tools/box-password`) live in files readable
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
src/daemon.rs         service body: iroh endpoint, master and agent sides of gangli/ctrl/1
src/client.rs         client commands (pair, nodes, info)
src/master.rs         state commands (master create/code/pair/nodes, join, leave, status)
src/proto.rs          control protocol messages and the request/answer helper
src/cert.rs code.rs   certificates; join / pairing codes
src/state.rs store.rs service state model; locked owner-only JSON files
src/inventory.rs      what a node reports (sysinfo)
src/elevate.rs        native elevation (UAC with output relay, osascript, pkexec/sudo)
src/service/          system service per OS (SCM, launchd, systemd)
docs/ARCHITECTURE.md  design and decisions
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
  (linux x86_64 + aarch64, glibc 2.28 via cargo-zigbuild), `windows-latest` (x86_64 msvc, static CRT),
  `macos-latest` (universal, ad-hoc signed).
- Toolchains (rustup, zig, cargo-zigbuild) and the target dir are kept in `$CACHE`, per OS.
- Logs come back in `out/box-<os>/build.log`.
- Push HEAD before a release build (the script checks it). The release `build-N` (N = newest + 1, also the
  version `0.0.N` baked into the binary through `GANGLI_BUILD`) of this **private** repository gets
  `gangli-linux-x86_64`, `gangli-linux-aarch64`, `gangli-windows-x86_64.exe`, `gangli-macos-universal` and
  `SHA256SUMS`.

## Testing

- Unit tests run on build-box as part of every build (`cargo test` on the native target of each runner).
- End to end on one PC, no elevation, no service: development mode with `GANGLI_STATE_DIR` (service state)
  and `GANGLI_CLIENT_DIR` (client state) pointing at scratch folders. Run a master and an agent as two
  `gangli run` processes with different state dirs, then pair a client and use `nodes` / `info`; the
  traffic goes through the real n0 relays and discovery.
- Network behaviour (drops, slow links) is tested with an in-process loss/latency shim on the resumable stream
  layer, not only by hand.
- Before a release that touches install / service code, try install → join → uninstall on all three OSes.
