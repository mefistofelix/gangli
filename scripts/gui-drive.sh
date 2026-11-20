#!/bin/bash
# Drives the Linux GUI under X11 (WSLg or Xvfb) with xdotool, for tests; screenshots with ImageMagick.
# usage: gui-drive.sh start | click X Y | dclick X Y | type TEXT | key KEYS | shot NAME [delay] | clip | setclip TEXT | stop
# The GUI runs from /tmp/gui1 (a console folder made by the test), with the binary of the last CI run (or GANGLI_BIN);
# screenshots go to out/x-NAME.png.
export DISPLAY=:0
R="$(cd "$(dirname "$0")/.." && pwd)"
WID=$(xdotool search --name '^gangli$' 2>/dev/null | head -1)
case "$1" in
start)
  pkill -f "gangli gui"; sleep 0.5
  cd /tmp/gui1; cp "${GANGLI_BIN:-$R/out/ci/linux/gangli-linux-x86_64}" gangli
  setsid nohup ./gangli gui > gui.out 2>&1 < /dev/null &
  for i in $(seq 1 40); do sleep 0.25; WID=$(xdotool search --name '^gangli$' 2>/dev/null | head -1); [ -n "$WID" ] && break; done
  sleep 1; xdotool windowmove "$WID" 0 0; echo "window $WID"; xdotool getwindowgeometry "$WID";;
click) xdotool mousemove --window "$WID" "$2" "$3" click 1;;
dclick) xdotool mousemove --window "$WID" "$2" "$3" click --repeat 2 1;;
type) xdotool type --delay 40 "$2";;
key) xdotool key --delay 40 "$2";;
shot) sleep "${3:-0.8}"; import -window "$WID" "$R/out/x-$2.png" && echo "saved x-$2.png";;
clip) xclip -o -selection clipboard 2>&1 | head -c 200; echo;;
setclip) printf "%s" "$2" > /tmp/clip.txt; setsid nohup xclip -i -selection clipboard -loops 3 /tmp/clip.txt > /dev/null 2>&1 < /dev/null & sleep 0.5;;
stop) pkill -f "gangli gui"; sleep 1; pgrep -fa "gangli gui" || echo stopped;;
esac
