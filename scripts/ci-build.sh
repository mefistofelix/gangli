#!/bin/bash
# gangli's build for one OS, on a runner of this repository's GitHub Actions (.github/workflows/build.yml).
# usage: ci-build.sh linux | windows | macos   (env: GANGLI_BUILD = N of the version 0.0.N, 0 when not a release)
# out/ci gets the binaries; ~/.gangli-ci keeps zig, cargo-zigbuild, the cargo registry and the target dir (cached
# between runs by the workflow).
set -e
OS="$1"
OUT="${OUT:-$PWD/out/ci}"; CACHE="${CACHE:-$HOME/.gangli-ci}"
mkdir -p "$CACHE" "$OUT"
export GANGLI_BUILD="${GANGLI_BUILD:-0}"
export CARGO_TARGET_DIR="$CACHE/target"
export CARGO_HOME="$CACHE/cargo-home"
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
mkdir -p "$CARGO_HOME"
# the runner images ship rustup; reuse its binaries from the default CARGO_HOME
export PATH="$CARGO_HOME/bin:$HOME/.cargo/bin:$PATH"
command -v rustup > /dev/null || { curl -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal; }
rustup toolchain install stable --profile minimal > /dev/null
rustup default stable > /dev/null
rustc --version

case "$OS" in
linux)
  # zig as the C toolchain / linker; musl, statically linked: the binaries need nothing on the system
  [ -x "$CACHE/venv/bin/python3" ] || python3 -m venv "$CACHE/venv"
  "$CACHE/venv/bin/python3" -c 'import ziglang' 2> /dev/null || "$CACHE/venv/bin/pip" install -q 'ziglang<0.15'
  export PATH="$CACHE/venv/bin:$PATH"
  [ -x "$CACHE/tools/bin/cargo-zigbuild" ] || cargo install -q --locked --root "$CACHE/tools" cargo-zigbuild
  export PATH="$CACHE/tools/bin:$PATH"
  rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl > /dev/null
  cargo test --release
  for A in x86_64 aarch64; do
    cargo zigbuild --release --target "$A-unknown-linux-musl"
    cp "$CARGO_TARGET_DIR/$A-unknown-linux-musl/release/gangli" "$OUT/gangli-linux-$A"
    file "$OUT/gangli-linux-$A" || true
  done
  ;;
windows)
  export RUSTFLAGS="-C target-feature=+crt-static"
  cargo test --release --target x86_64-pc-windows-msvc
  cargo build --release --target x86_64-pc-windows-msvc
  cp "$CARGO_TARGET_DIR/x86_64-pc-windows-msvc/release/gangli.exe" "$OUT/gangli-windows-x86_64.exe"
  ;;
macos)
  rustup target add aarch64-apple-darwin x86_64-apple-darwin > /dev/null
  export MACOSX_DEPLOYMENT_TARGET=11.0
  cargo test --release
  for A in aarch64 x86_64; do cargo build --release --target "$A-apple-darwin"; done
  lipo -create -output "$OUT/gangli-macos-universal" \
    "$CARGO_TARGET_DIR/aarch64-apple-darwin/release/gangli" "$CARGO_TARGET_DIR/x86_64-apple-darwin/release/gangli"
  codesign --force -s - "$OUT/gangli-macos-universal"
  ;;
*) echo "usage: ci-build.sh linux | windows | macos"; exit 2;;
esac
ls -la "$OUT"
