#!/usr/bin/env python3
"""Linux tray test without a desktop: a minimal StatusNotifierWatcher + host on the D-Bus session bus.

When gangli's tray registers, it prints the item's title, tooltip, icon sizes and menu, then (with --click
LABEL) activates the menu entry with that label, or (with --activate) left-clicks the icon. Needs
python3-dbus and python3-gi. Used in WSL: start this, then `gangli gui`.

usage: tray-test.py [--click LABEL] [--activate] [--timeout SECONDS]
"""
import argparse
import sys

import dbus
import dbus.service
from dbus.mainloop.glib import DBusGMainLoop
from gi.repository import GLib

WATCHER = "org.kde.StatusNotifierWatcher"
ITEM = "org.kde.StatusNotifierItem"
MENU = "com.canonical.dbusmenu"

args = argparse.ArgumentParser()
args.add_argument("--click")
args.add_argument("--activate", action="store_true")
args.add_argument("--timeout", type=int, default=30)
opts = args.parse_args()

DBusGMainLoop(set_as_default=True)
bus = dbus.SessionBus()
loop = GLib.MainLoop()


def labels(layout, depth=0, out=None):
    out = [] if out is None else out
    ident, props, children = layout
    label = str(props.get("label", ""))
    if props.get("type") == "separator":
        label = "---"
    if depth > 0:
        out.append((int(ident), depth, label, bool(props.get("enabled", True))))
    for c in children:
        labels(c, depth + 1, out)
    return out


def inspect(service):
    name, _, path = service.partition("/")
    path = "/" + path if path else "/StatusNotifierItem"
    item = bus.get_object(name, path)
    props = dbus.Interface(item, "org.freedesktop.DBus.Properties")
    title = props.Get(ITEM, "Title")
    tooltip = props.Get(ITEM, "ToolTip")
    icons = props.Get(ITEM, "IconPixmap")
    print(f"item {service}: title={title!r} tooltip={str(tooltip[2])!r} icons={[f'{w}x{h}' for w, h, _ in icons]}")
    menu_path = props.Get(ITEM, "Menu")
    menu = dbus.Interface(bus.get_object(name, menu_path), MENU)
    _, layout = menu.GetLayout(0, -1, [])
    entries = labels(layout)
    for ident, depth, label, enabled in entries:
        print("  " * depth + f"[{ident}] {label}" + ("" if enabled else " (disabled)"))
    if opts.activate:
        dbus.Interface(item, ITEM).Activate(0, 0)
        print("activated")
    if opts.click:
        hit = [e for e in entries if e[2].replace("_", "") == opts.click]
        if not hit:
            print(f"no menu entry {opts.click!r}")
        else:
            menu.Event(hit[0][0], "clicked", dbus.String("", variant_level=1), dbus.UInt32(0))
            print(f"clicked {opts.click!r}")
    sys.stdout.flush()
    GLib.timeout_add(1500, loop.quit)


class Watcher(dbus.service.Object):
    def __init__(self):
        name = dbus.service.BusName(WATCHER, bus)
        super().__init__(name, "/StatusNotifierWatcher")
        self.items = []

    @dbus.service.method(WATCHER, in_signature="s", sender_keyword="sender")
    def RegisterStatusNotifierItem(self, service, sender=None):
        full = service if service.startswith((":", "org.")) else sender + service
        self.items.append(full)
        self.StatusNotifierItemRegistered(full)
        GLib.timeout_add(500, lambda: inspect(full) and False)

    @dbus.service.method(WATCHER, in_signature="s")
    def RegisterStatusNotifierHost(self, service):
        pass

    @dbus.service.signal(WATCHER, signature="s")
    def StatusNotifierItemRegistered(self, service):
        pass

    @dbus.service.method("org.freedesktop.DBus.Properties", in_signature="ss", out_signature="v")
    def Get(self, interface, prop):
        return self.GetAll(interface)[prop]

    @dbus.service.method("org.freedesktop.DBus.Properties", in_signature="s", out_signature="a{sv}")
    def GetAll(self, interface):
        return {
            "RegisteredStatusNotifierItems": dbus.Array(self.items, signature="s"),
            "IsStatusNotifierHostRegistered": dbus.Boolean(True),
            "ProtocolVersion": dbus.Int32(0),
        }


Watcher()
print("watcher ready")
sys.stdout.flush()
GLib.timeout_add_seconds(opts.timeout, loop.quit)
loop.run()
