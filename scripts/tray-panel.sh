#!/bin/bash
# A real Linux tray for tests in WSLg: xfce4-panel with only its notification area (StatusNotifier host and
# watcher), on X11. Then `gangli gui` shows its icon there; drive it with xdotool (left click opens the window,
# right click the menu, a "wrapper-2.0" window). Needs xfce4-panel, xfconf, dbus-x11 (apt, once).
# usage: tray-panel.sh start | stop
export DISPLAY="${DISPLAY:-:0}"
case "$1" in
start)
  mkdir -p ~/.config/xfce4/xfconf/xfce-perchannel-xml
  cat > ~/.config/xfce4/xfconf/xfce-perchannel-xml/xfce4-panel.xml <<'XML'
<?xml version="1.0" encoding="UTF-8"?>
<channel name="xfce4-panel" version="1.0">
  <property name="configver" type="int" value="2"/>
  <property name="panels" type="array">
    <value type="int" value="1"/>
    <property name="panel-1" type="empty">
      <property name="position" type="string" value="p=6;x=0;y=0"/>
      <property name="length" type="uint" value="30"/>
      <property name="size" type="uint" value="36"/>
      <property name="plugin-ids" type="array"><value type="int" value="1"/></property>
    </property>
  </property>
  <property name="plugins" type="empty"><property name="plugin-1" type="string" value="systray"/></property>
</channel>
XML
  pkill -x xfce4-panel; pkill -x xfconfd; sleep 0.3
  # GTK would pick Wayland under WSLg; the panel needs X11.
  GDK_BACKEND=x11 setsid nohup xfce4-panel > /tmp/tray-panel.out 2>&1 < /dev/null &
  sleep 3
  xdotool search --class xfce4-panel | while read -r w; do xdotool getwindowgeometry "$w" | tr '\n' ' '; echo; done;;
stop) pkill -x xfce4-panel; pkill -x xfconfd;;
*) echo "usage: tray-panel.sh start | stop"; exit 2;;
esac
