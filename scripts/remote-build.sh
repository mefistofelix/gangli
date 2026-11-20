#!/bin/bash
# Builds gangli for Linux, Windows and macOS on the blind build machine (public repository mefistofelix/build-box,
# cloned in .tools/build-box; its README.md and AGENTS.md explain it) and publishes the binaries as the GitHub
# release build-N of this private repository. The working tree (tracked and new files, not the ignored ones) is
# staged with a build.sh that runs scripts/box-build.sh, and box.sh sends it encrypted with the password in
# .tools/box-password (the secret BOX_PASSWORD of build-box). The three OSes build in parallel; logs come back
# in out/box-<os>/build.log. Run from the project root in Git Bash.
# usage: bash scripts/remote-build.sh [--no-release] [--only "linux windows macos"]
set -e
RELEASE=1; ONLY="linux windows macos"
while [ $# -gt 0 ]; do
  case "$1" in --no-release) RELEASE=0; shift;; --only) ONLY="$2"; shift 2;; *) echo "unknown option $1"; exit 2;; esac
done
[ -f .tools/build-box/box.sh ] || git clone -q https://github.com/mefistofelix/build-box .tools/build-box
[ -f .tools/box-password ] || { echo "missing .tools/box-password"; exit 2; }
if [ "$RELEASE" = 1 ]; then
  # the release points at HEAD, which GitHub must already have
  git fetch -q origin && git merge-base --is-ancestor HEAD "@{u}" || { echo "push HEAD first (git push)"; exit 2; }
  LAST=$(gh release list -L 50 --json tagName -q '.[].tagName' | sed -n 's/^build-//p' | sort -n | tail -1)
  N=$(( ${LAST:-0} + 1 ))
else N=0; fi
W="$(mktemp -d)"; trap 'rm -rf "$W"' EXIT
declare -A RUNNER=([linux]=ubuntu-latest [windows]=windows-latest [macos]=macos-latest)
declare -A PID
for OS in $ONLY; do
  S="$W/src-$OS"; mkdir -p "$S"
  git ls-files -co --exclude-standard -z | tar --null -T - -cf - | tar -xf - -C "$S"
  printf '%s\n' "$N" > "$S/build-number"; printf '%s\n' "$OS" > "$S/build-target"
  # the public half of the release key goes into the binaries (they then accept signed updates)
  [ -f .tools/release.pub ] && tr -d ' \r\n' < .tools/release.pub > "$S/release-pub"
  printf '#!/bin/bash\nset -e\nexec bash scripts/box-build.sh\n' > "$S/build.sh"
  R="out/box-$OS"; rm -rf "$R"
  bash .tools/build-box/box.sh "$S" .tools/box-password "$R" --runner "${RUNNER[$OS]}" --cache "gangli-$OS" \
    --cache-save always --timeout 60 > "$W/$OS.txt" 2>&1 &
  PID[$OS]=$!
done
FAIL=0
for OS in $ONLY; do
  if wait "${PID[$OS]}"; then echo "$OS: ok"; else echo "$OS: FAILED"; FAIL=1; fi
  sed 's/^/  /' "$W/$OS.txt"
done
[ "$FAIL" = 0 ] || exit 1
mkdir -p out/release; rm -f out/release/*
for OS in $ONLY; do cp out/box-$OS/gangli-* out/release/; done
# keep the lock file the build resolved (same on every OS for the same tree)
for OS in $ONLY; do [ -f out/box-$OS/Cargo.lock ] && cp out/box-$OS/Cargo.lock Cargo.lock && break; done
(cd out/release && sha256sum gangli-* > SHA256SUMS)
# sign every binary with the owner's release key (.tools/release.key), using the binary for this computer
if [ -f .tools/release.key ]; then
  case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) HOSTBIN=out/release/gangli-windows-x86_64.exe;; Darwin) HOSTBIN=out/release/gangli-macos-universal;; *) HOSTBIN=out/release/gangli-linux-$(uname -m);; esac
  if [ -x "$HOSTBIN" ] || [ -f "$HOSTBIN" ]; then "$HOSTBIN" release sign --key .tools/release.key out/release/gangli-*[!g] && echo "signed"; fi
fi
ls -la out/release
[ "$RELEASE" = 1 ] || exit 0
gh release create "build-$N" out/release/* --target "$(git rev-parse HEAD)" --title "gangli build $N" \
  --notes "Built on build-box from $(git rev-parse --short HEAD): $(git log -1 --pretty=%s)" --latest > /dev/null
echo "release build-$N"
