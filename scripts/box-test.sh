#!/bin/bash
# End-to-end test on the build machine, right after scripts/box-build.sh, when the archive has a `build-test`
# file (scripts/remote-build.sh --test). The binary just built runs as a real service (the runners have
# passwordless sudo / an elevated account) with a real console, through the real relays: install, the
# machine comes online, exec as the service account, the GUI (screenshot), update (the service restarts
# with the binary it receives), uninstall. Short on purpose: runner minutes, the macOS ones above all.
# Output: $OUT/test.log (this script's output), $OUT/gui-<os>.png, a last line "TESTS: n passed, m failed".
set -u
OS="$(cat build-target)"
case "$OS" in
  linux) BIN="$OUT/gangli-linux-x86_64"; EXE=gangli;;
  windows) BIN="$OUT/gangli-windows-x86_64.exe"; EXE=gangli.exe;;
  macos) BIN="$OUT/gangli-macos-universal"; EXE=gangli;;
esac
T="$(mktemp -d)"; C="$T/console"; N="$T/node"; mkdir -p "$C" "$N"
cp "$BIN" "$C/$EXE"; cp "$BIN" "$N/$EXE"; chmod +x "$C/$EXE" "$N/$EXE"
G="$C/$EXE"
PASS=0; FAIL=0
ok() { echo "PASS $*"; PASS=$((PASS + 1)); }
ko() { echo "FAIL $*"; FAIL=$((FAIL + 1)); }
step() { echo; echo "== $*"; }
# Elevated commands: sudo on Unix; on Windows the runner account is already an administrator.
admin() { if [ "$OS" = windows ]; then "$@"; else sudo -n "$@"; fi; }
# `timeout` without GNU coreutils (macOS has none). The sleeper writes nowhere and dies with it: a process left
# holding test.log open would stop the build machine from sealing the result (Windows).
tmo() {
  local s=$1; shift
  "$@" &
  local p=$!
  sleep "$s" > /dev/null 2>&1 < /dev/null &
  local k=$!
  while kill -0 $p 2> /dev/null; do
    kill -0 $k 2> /dev/null || { kill $p 2> /dev/null; break; }
    sleep 1
  done
  wait $p; local r=$?
  kill $k 2> /dev/null; wait $k 2> /dev/null
  return $r
}
note() { echo "NOTE $*"; }
online() { "$G" nodes 2>/dev/null | grep -q " online "; }
wait_online() { for _ in $(seq 1 ${1:-60}); do online && return 0; sleep 1; done; return 1; }

step version
"$G" version && ok version || ko version

step "network and console"
PUB=$("$G" --json network create test | sed -n 's/.*"public_code": *"\([^"]*\)".*/\1/p')
[ -n "$PUB" ] && ok "network create" || ko "network create"
(cd "$C" && "./$EXE" console run > "$T/console.out" 2>&1 < /dev/null &)
sleep 3
"$G" console status && ok "console open" || ko "console open"

step "install the node as a service"
if tmo 120 admin "$N/$EXE" install --code "$PUB"; then ok install; else ko install; fi
if wait_online 90; then ok "service node online"; else ko "service node online"; fi
"$G" nodes
NODE=$("$G" --json nodes | sed -n 's/.*"name": *"\([^"]*\)".*/\1/p' | head -1)
echo "machine: $NODE"

step "exec as the service account"
WHO=$("$G" exec "$NODE" --user system -- whoami 2>&1 | tr -d '\r'); echo "whoami as system: $WHO"
echo "whoami as default (the desktop user, if any): $("$G" exec "$NODE" -- whoami 2>&1 | tr -d '\r')"
case "$WHO" in root|*system*|*SYSTEM*) ok "exec as root / SYSTEM";; *) ko "exec as root / SYSTEM ($WHO)";; esac
"$G" exec "$NODE" -- echo hello-from-the-service | grep -q hello-from-the-service && ok "exec output" || ko "exec output"

step "GUI (screenshot)"
SHOT="$OUT/gui-$OS.png"
export GANGLI_GUI_SCREENSHOT="$SHOT" GANGLI_GUI_SCREENSHOT_AFTER=6 GANGLI_LANG=it
case "$OS" in
  linux) (cd "$C" && tmo 60 xvfb-run -a -s "-screen 0 1280x800x24" "./$EXE" gui) ;;
  *) (cd "$C" && tmo 60 "./$EXE" gui) ;;
esac
echo "gui exit: $?"; unset GANGLI_GUI_SCREENSHOT
# The runners may have no usable OpenGL (Windows, macOS VMs): reported, not a failure.
[ -s "$SHOT" ] && ok "GUI screenshot" || note "no GUI screenshot on this runner"
grep "gui:" "$C/gangli.log" | tail -3

step "update (the service restarts with the binary it receives)"
runs() { admin cat "$N/gangli.log" 2> /dev/null | grep -c "node running"; }
BEFORE=$(runs)
"$G" update "$NODE" --from "$OUT" && ok "update sent" || ko "update sent"
# The service manager must start the new binary: one more "node running" in the node's log.
for _ in $(seq 1 60); do [ "$(runs)" -gt "$BEFORE" ] && break; sleep 1; done
[ "$(runs)" -gt "$BEFORE" ] && ok "service restarted with the new binary" || ko "service restarted with the new binary"
sleep 3
if wait_online 90; then ok "back online after update"; else ko "back online after update"; fi

step uninstall
if tmo 120 admin "$N/$EXE" uninstall; then ok uninstall; else ko uninstall; fi
sleep 3
case "$OS" in
  linux) systemctl is-active gangli && ko "service gone" || ok "service gone";;
  macos) sudo -n launchctl print system/com.mefistofelix.gangli > /dev/null 2>&1 && ko "service gone" || ok "service gone";;
  windows) sc.exe query gangli > /dev/null 2>&1 && ko "service gone" || ok "service gone";;
esac

step logs
"$G" console stop
echo "--- node log"; admin cat "$N/gangli.log" 2>/dev/null | tail -15
echo "--- console log"; tail -15 "$C/gangli.log"
# Nothing of ours may outlive the test (open files would block sealing the result).
[ "$OS" = windows ] && taskkill //F //IM gangli.exe > /dev/null 2>&1
echo
echo "TESTS: $PASS passed, $FAIL failed"
