#!/bin/bash
# Local build in WSL for quick checks between build-box runs (never for a release): Linux x86_64 static musl
# and Windows x86_64 (gnu toolchain, static), into out/wsl/. Needs rustup with the targets
# x86_64-unknown-linux-musl and x86_64-pc-windows-gnu, musl-tools and mingw-w64 in the WSL distribution.
# The sources are copied to ~/gangli-src and built in ~/gangli-target (the Linux file system: much faster).
# With .tools/release.pub the binaries get the release key; with --sign they are signed with .tools/release.key
# like a release (to test updates).
# usage, from the project root: wsl.exe -d Ubuntu -- bash scripts/wsl-build.sh [linux] [windows] [--build N] [--sign] [--test]
set -e
TARGETS=""; N=0; SIGN=0; TEST=0
while [ $# -gt 0 ]; do
  case "$1" in linux|windows) TARGETS="$TARGETS $1"; shift;; --build) N="$2"; shift 2;; --sign) SIGN=1; shift;;
    --test) TEST=1; shift;; *) echo "unknown option $1"; exit 2;; esac
done
[ -n "$TARGETS" ] || TARGETS="linux windows"
[ -f ~/.cargo/env ] && . ~/.cargo/env
ROOT="$(pwd)"; SRC=~/gangli-src
mkdir -p "$SRC"; rm -rf "$SRC/src" "$SRC/scripts" "$SRC/docs"
tar --exclude=./out --exclude=./.tools --exclude=./.git --exclude=./target -cf - . | tar -xf - -C "$SRC"
export GANGLI_BUILD="$N" CARGO_TARGET_DIR=~/gangli-target
[ -f .tools/release.pub ] && export GANGLI_RELEASE_PUB="$(tr -d ' \r\n' < .tools/release.pub)"
# faster than the release profile; the code is the same
export CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc
mkdir -p "$ROOT/out/wsl"; cd "$SRC"
export COLUMNS=200
[ "$TEST" = 1 ] && cargo test --release
for T in $TARGETS; do
  case "$T" in
  linux)
    cargo build --release --target x86_64-unknown-linux-musl
    cp "$CARGO_TARGET_DIR/x86_64-unknown-linux-musl/release/gangli" "$ROOT/out/wsl/gangli-linux-x86_64";;
  windows)
    RUSTFLAGS="-C target-feature=+crt-static" cargo build --release --target x86_64-pc-windows-gnu
    cp "$CARGO_TARGET_DIR/x86_64-pc-windows-gnu/release/gangli.exe" "$ROOT/out/wsl/gangli-windows-x86_64.exe";;
  esac
done
cp Cargo.lock "$ROOT/out/wsl/Cargo.lock"
cd "$ROOT"
if [ "$SIGN" = 1 ] && [ -f .tools/release.key ]; then
  B=out/wsl/gangli-linux-x86_64; [ -f "$B" ] || B="$CARGO_TARGET_DIR/x86_64-unknown-linux-musl/release/gangli"
  "$B" release sign --key .tools/release.key out/wsl/gangli-*[!gk] && echo "signed"
fi
ls -la out/wsl
