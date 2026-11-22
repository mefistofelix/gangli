//! Linux: the window over the X11 protocol in pure Rust (x11rb's own connection, no libX11; XWayland on Wayland
//! desktops), egui drawn by `raster.rs` and sent with PutImage. Keyboard from the server's keymap (with AltGr
//! levels, so Italian and other layouts type right), CLIPBOARD selection both ways, cursors from the cursor
//! font, scale from `Xft.dpi` (or `GANGLI_GUI_SCALE`).
//!
//! `GANGLI_GUI_SCREENSHOT=<file.png>` saves the window as drawn after `GANGLI_GUI_SCREENSHOT_AFTER` seconds
//! (default 4) and quits: the GUI test.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use egui::{CursorIcon, Event, Key, Modifiers, PointerButton, Pos2, ViewportId};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, ImageFormat, KeyButMask,
    PropMode, SelectionNotifyEvent, StackMode, Window, WindowClass, SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event as XEvent;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::CURRENT_TIME;

use super::client::Client;
use super::raster::Raster;
use super::{App, TrayAction};

enum Msg {
    X(XEvent),
    Gone(String),
    Wake,
    Tray(TrayAction),
}

struct Atoms {
    protocols: Atom,
    delete: Atom,
    net_wm_name: Atom,
    utf8: Atom,
    net_wm_icon: Atom,
    clipboard: Atom,
    targets: Atom,
    paste: Atom,
    text: Atom,
    active: Atom,
    net_wm_pid: Atom,
}

fn atom(c: &RustConnection, name: &str) -> Result<Atom> {
    Ok(c.intern_atom(false, name.as_bytes())?.reply()?.atom)
}

struct Keymap {
    min: u8,
    per: usize,
    syms: Vec<u32>,
}

impl Keymap {
    fn sym(&self, code: u8, idx: usize) -> u32 {
        if code < self.min || idx >= self.per {
            return 0;
        }
        self.syms.get((code - self.min) as usize * self.per + idx).copied().unwrap_or(0)
    }

    /// The keysym for a key press with these modifier bits (Shift, Lock, NumLock = Mod2, AltGr = Mod5).
    fn lookup(&self, code: u8, state: u16) -> u32 {
        let shift = state & u16::from(KeyButMask::SHIFT) != 0;
        let lock = state & u16::from(KeyButMask::LOCK) != 0;
        let numlock = state & u16::from(KeyButMask::MOD2) != 0;
        let altgr = state & u16::from(KeyButMask::MOD5) != 0;
        let base = if altgr && self.sym(code, 4) != 0 { 4 } else { 0 };
        let plain = self.sym(code, base);
        let shifted = self.sym(code, base + 1);
        // Keypad: NumLock picks the digits.
        if (0xff80..=0xffbd).contains(&shifted) {
            return if numlock != shift { shifted } else { plain };
        }
        let letter = |s: u32| (0x61..=0x7a).contains(&s) || (0xe0..=0xfe).contains(&s) && s != 0xf7;
        let up = shift != (lock && letter(plain));
        if up {
            if shifted != 0 {
                shifted
            } else if letter(plain) {
                plain - 0x20
            } else {
                plain
            }
        } else {
            plain
        }
    }
}

/// The server's keymap (again after a MappingNotify: layout switched).
fn keymap(c: &RustConnection) -> Result<Keymap> {
    let setup = c.setup();
    let (min, max) = (setup.min_keycode, setup.max_keycode);
    let km = c.get_keyboard_mapping(min, max - min + 1)?.reply()?;
    Ok(Keymap { min, per: km.keysyms_per_keycode as usize, syms: km.keysyms })
}

fn keysym_char(s: u32) -> Option<char> {
    match s {
        0x20..=0x7e | 0xa0..=0xff => char::from_u32(s),
        0x0100_0100..=0x0110_ffff => char::from_u32(s - 0x0100_0000),
        0x20ac => Some('€'),
        0xffaa => Some('*'),
        0xffab => Some('+'),
        0xffad => Some('-'),
        0xffae => Some('.'),
        0xffaf => Some('/'),
        0xffb0..=0xffb9 => char::from_u32(s - 0xffb0 + '0' as u32),
        _ => None,
    }
}

fn keysym_key(s: u32) -> Option<Key> {
    Some(match s {
        0xff08 => Key::Backspace,
        0xff09 | 0xfe20 => Key::Tab,
        0xff0d | 0xff8d => Key::Enter,
        0xff1b => Key::Escape,
        0xffff | 0xff9f => Key::Delete,
        0xff50 | 0xff95 => Key::Home,
        0xff51 | 0xff96 => Key::ArrowLeft,
        0xff52 | 0xff97 => Key::ArrowUp,
        0xff53 | 0xff98 => Key::ArrowRight,
        0xff54 | 0xff99 => Key::ArrowDown,
        0xff55 | 0xff9a => Key::PageUp,
        0xff56 | 0xff9b => Key::PageDown,
        0xff57 | 0xff9c => Key::End,
        0xff63 | 0xff9e => Key::Insert,
        0x20 => Key::Space,
        0xffbe..=0xffc9 => Key::from_name(&format!("F{}", s - 0xffbe + 1))?,
        0x61..=0x7a => Key::from_name(&char::from_u32(s - 0x20)?.to_string())?,
        0x41..=0x5a | 0x30..=0x39 => Key::from_name(&char::from_u32(s)?.to_string())?,
        0xffb0..=0xffb9 => Key::from_name(&char::from_u32(s - 0xffb0 + '0' as u32)?.to_string())?,
        0x2d | 0xffad => Key::Minus,
        0x2b | 0xffab => Key::Plus,
        0x3d => Key::Equals,
        _ => return None,
    })
}

fn modifiers(state: u16) -> Modifiers {
    let ctrl = state & u16::from(KeyButMask::CONTROL) != 0;
    Modifiers {
        alt: state & u16::from(KeyButMask::MOD1) != 0,
        ctrl,
        shift: state & u16::from(KeyButMask::SHIFT) != 0,
        mac_cmd: false,
        command: ctrl,
    }
}

/// `Xft.dpi` from the root window's resources, as a scale factor (96 dpi = 1).
fn dpi_scale(c: &RustConnection, root: Window) -> f32 {
    if let Some(s) = std::env::var("GANGLI_GUI_SCALE").ok().and_then(|s| s.parse::<f32>().ok()) {
        return s.clamp(0.5, 4.0);
    }
    let r = c.get_property(false, root, AtomEnum::RESOURCE_MANAGER, AtomEnum::STRING, 0, 1 << 16).ok().and_then(|c| c.reply().ok());
    if let Some(r) = r {
        for line in String::from_utf8_lossy(&r.value).lines() {
            if let Some(v) = line.strip_prefix("Xft.dpi:") {
                if let Ok(d) = v.trim().parse::<f32>() {
                    return (d / 96.0).clamp(0.5, 4.0);
                }
            }
        }
    }
    1.0
}

struct Win {
    c: Arc<RustConnection>,
    win: Window,
    root: Window,
    gc: u32,
    depth: u8,
    a: Atoms,
    keys: Keymap,
    cursor_font: u32,
    cursors: HashMap<u16, u32>,
    cursor_now: u16,
    clip_text: Option<String>,
    mapped: bool,
}

impl Win {
    fn set_cursor(&mut self, icon: CursorIcon) {
        let glyph = match icon {
            CursorIcon::PointingHand => 60,
            CursorIcon::Text | CursorIcon::VerticalText => 152,
            CursorIcon::ResizeHorizontal | CursorIcon::ResizeColumn | CursorIcon::ResizeEast | CursorIcon::ResizeWest => 108,
            CursorIcon::ResizeVertical | CursorIcon::ResizeRow | CursorIcon::ResizeNorth | CursorIcon::ResizeSouth => 116,
            CursorIcon::Grab | CursorIcon::Grabbing | CursorIcon::Move | CursorIcon::AllScroll => 52,
            CursorIcon::Wait | CursorIcon::Progress => 150,
            _ => 68,
        };
        if glyph == self.cursor_now {
            return;
        }
        self.cursor_now = glyph;
        let id = match self.cursors.get(&glyph) {
            Some(id) => *id,
            None => {
                let Ok(id) = self.c.generate_id() else { return };
                if self.c.create_glyph_cursor(id, self.cursor_font, self.cursor_font, glyph, glyph + 1, 0, 0, 0, 0xffff, 0xffff, 0xffff).is_err() {
                    return;
                }
                self.cursors.insert(glyph, id);
                id
            }
        };
        let _ = self.c.change_window_attributes(self.win, &ChangeWindowAttributesAux::new().cursor(id));
    }

    fn show(&mut self) {
        let _ = self.c.map_window(self.win);
        let _ = self.c.configure_window(self.win, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE));
        let ev = ClientMessageEvent::new(32, self.win, self.a.active, [1u32, 0, 0, 0, 0]);
        let _ = self.c.send_event(false, self.root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, ev);
        let _ = self.c.flush();
    }

    fn hide(&mut self) {
        let _ = self.c.unmap_window(self.win);
        let _ = self.c.flush();
    }

    fn copy(&mut self, text: String) {
        self.clip_text = Some(text);
        let _ = self.c.set_selection_owner(self.win, self.a.clipboard, CURRENT_TIME);
        let _ = self.c.flush();
    }

    fn request_paste(&self) {
        let _ = self.c.convert_selection(self.win, self.a.clipboard, self.a.utf8, self.a.paste, CURRENT_TIME);
        let _ = self.c.flush();
    }

    /// Another program asks for our CLIPBOARD.
    fn answer_selection(&self, e: &x11rb::protocol::xproto::SelectionRequestEvent) {
        let mut property = e.property;
        match &self.clip_text {
            Some(_) if e.target == self.a.targets => {
                let _ = self.c.change_property32(PropMode::REPLACE, e.requestor, e.property, AtomEnum::ATOM, &[self.a.targets, self.a.utf8, u32::from(AtomEnum::STRING), self.a.text]);
            }
            Some(t) if e.target == self.a.utf8 || e.target == u32::from(AtomEnum::STRING) || e.target == self.a.text => {
                let _ = self.c.change_property8(PropMode::REPLACE, e.requestor, e.property, e.target, t.as_bytes());
            }
            _ => property = x11rb::NONE,
        }
        let ev = SelectionNotifyEvent { response_type: SELECTION_NOTIFY_EVENT, sequence: 0, time: e.time, requestor: e.requestor, selection: e.selection, target: e.target, property };
        let _ = self.c.send_event(false, e.requestor, EventMask::NO_EVENT, ev);
        let _ = self.c.flush();
    }

    /// Send the frame buffer, in strips small enough for one request.
    fn put(&self, r: &Raster, bytes: &mut Vec<u8>) -> Result<()> {
        if r.w == 0 || r.h == 0 {
            return Ok(());
        }
        let max = self.c.maximum_request_bytes().saturating_sub(64).max(4096);
        let rows = (max / (r.w * 4)).clamp(1, r.h);
        let mut y = 0;
        while y < r.h {
            let n = rows.min(r.h - y);
            bytes.clear();
            for p in &r.buf[y * r.w..(y + n) * r.w] {
                bytes.extend_from_slice(&p.to_le_bytes());
            }
            self.c.put_image(ImageFormat::Z_PIXMAP, self.win, self.gc, r.w as u16, n as u16, 0, y as i16, 0, self.depth, bytes)?;
            y += n;
        }
        self.c.flush()?;
        Ok(())
    }
}

pub fn run(client: Client, hidden: bool) -> Result<()> {
    let display = std::env::var("DISPLAY").unwrap_or_default();
    let (c, screen_num) = RustConnection::connect(None).map_err(|e| {
        anyhow!("no X11 display to open the window on (DISPLAY={display:?}: {e}); the commands work without it, see `gangli --help`")
    })?;
    let c = Arc::new(c);
    let screen = c.setup().roots[screen_num].clone();
    let depth = screen.root_depth;
    let visual = screen
        .allowed_depths
        .iter()
        .flat_map(|d| d.visuals.iter())
        .find(|v| v.visual_id == screen.root_visual)
        .cloned()
        .context("no root visual")?;
    if depth < 24 || visual.red_mask != 0xff0000 || visual.green_mask != 0xff00 || visual.blue_mask != 0xff {
        bail!("the X server's screen is not 24-bit RGB (depth {depth}): not supported");
    }
    let a = Atoms {
        protocols: atom(&c, "WM_PROTOCOLS")?,
        delete: atom(&c, "WM_DELETE_WINDOW")?,
        net_wm_name: atom(&c, "_NET_WM_NAME")?,
        utf8: atom(&c, "UTF8_STRING")?,
        net_wm_icon: atom(&c, "_NET_WM_ICON")?,
        clipboard: atom(&c, "CLIPBOARD")?,
        targets: atom(&c, "TARGETS")?,
        paste: atom(&c, "GANGLI_PASTE")?,
        text: atom(&c, "TEXT")?,
        active: atom(&c, "_NET_ACTIVE_WINDOW")?,
        net_wm_pid: atom(&c, "_NET_WM_PID")?,
    };
    let scale = dpi_scale(&c, screen.root);
    // The last size and place, else about three quarters of the screen, centered.
    let (sw, sh) = (screen.width_in_pixels as f32 / scale, screen.height_in_pixels as f32 / scale);
    let g = super::saved_geometry().unwrap_or_else(|| {
        let (w, h) = super::default_size(sw, sh);
        super::Geometry { x: ((sw - w) / 2.0).max(0.0), y: ((sh - h) / 2.0).max(0.0), w, h }
    });
    let init = ((g.w * scale) as u16, (g.h * scale) as u16);
    let pos = ((g.x * scale) as i16, (g.y * scale) as i16);
    let win = c.generate_id()?;
    let mask = EventMask::EXPOSURE
        | EventMask::STRUCTURE_NOTIFY
        | EventMask::KEY_PRESS
        | EventMask::KEY_RELEASE
        | EventMask::BUTTON_PRESS
        | EventMask::BUTTON_RELEASE
        | EventMask::POINTER_MOTION
        | EventMask::FOCUS_CHANGE
        | EventMask::LEAVE_WINDOW;
    c.create_window(depth, win, screen.root, pos.0, pos.1, init.0, init.1, 0, WindowClass::INPUT_OUTPUT, screen.root_visual, &CreateWindowAux::new().background_pixel(0x202020).event_mask(mask))?;
    c.change_property8(PropMode::REPLACE, win, AtomEnum::WM_NAME, AtomEnum::STRING, b"gangli")?;
    c.change_property8(PropMode::REPLACE, win, a.net_wm_name, a.utf8, b"gangli")?;
    c.change_property8(PropMode::REPLACE, win, AtomEnum::WM_CLASS, AtomEnum::STRING, b"gangli\0gangli\0")?;
    c.change_property32(PropMode::REPLACE, win, a.protocols, AtomEnum::ATOM, &[a.delete])?;
    c.change_property32(PropMode::REPLACE, win, a.net_wm_pid, AtomEnum::CARDINAL, &[std::process::id()])?;
    let mut icon = vec![];
    for size in [32u32, 64] {
        icon.extend_from_slice(&[size, size]);
        for p in super::icon::rgba(size).chunks_exact(4) {
            icon.push(((p[3] as u32) << 24) | ((p[0] as u32) << 16) | ((p[1] as u32) << 8) | p[2] as u32);
        }
    }
    c.change_property32(PropMode::REPLACE, win, a.net_wm_icon, AtomEnum::CARDINAL, &icon)?;
    // WM_NORMAL_HINTS: our place and size (USPosition | USSize, fields 1-4), the minimum size (PMinSize, fields
    // 5-6), and StaticGravity (PWinGravity, field 17): the place is the window's own, not its frame's, so a
    // saved place comes back exactly instead of moving down by the title bar at every start.
    let mut hints = [0u32; 18];
    hints[0] = 1 | 2 | 16 | 512;
    hints[1] = pos.0 as i32 as u32;
    hints[2] = pos.1 as i32 as u32;
    hints[3] = init.0 as u32;
    hints[4] = init.1 as u32;
    hints[5] = (super::MIN_SIZE.0 * scale) as u32;
    hints[6] = (super::MIN_SIZE.1 * scale) as u32;
    hints[17] = 10;
    c.change_property32(PropMode::REPLACE, win, AtomEnum::WM_NORMAL_HINTS, AtomEnum::WM_SIZE_HINTS, &hints)?;
    let gc = c.generate_id()?;
    c.create_gc(gc, win, &CreateGCAux::new().graphics_exposures(0))?;
    let cursor_font = c.generate_id()?;
    c.open_font(cursor_font, b"cursor")?;
    let keys = keymap(&c)?;
    if !hidden {
        c.map_window(win)?;
    }
    c.flush()?;

    let (tx, rx) = mpsc::channel::<Msg>();
    {
        let (c2, tx2) = (c.clone(), tx.clone());
        std::thread::spawn(move || loop {
            match c2.wait_for_event() {
                Ok(e) => {
                    if tx2.send(Msg::X(e)).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx2.send(Msg::Gone(e.to_string()));
                    break;
                }
            }
        });
    }
    let mut app = App::new(client);
    {
        let tx2 = std::sync::Mutex::new(tx.clone());
        app.set_waker(Arc::new(move || {
            let _ = tx2.lock().unwrap().send(Msg::Wake);
        }));
    }
    let tray_ok = Arc::new(AtomicBool::new(false));
    let tray_tx = {
        let tx2 = std::sync::Mutex::new(tx.clone());
        super::sni::spawn(app.tray_model(), Arc::new(move |a| {
            let _ = tx2.lock().unwrap().send(Msg::Tray(a));
        }), tray_ok.clone())
    };
    let mut model = app.tray_model();

    let mut w = Win { c: c.clone(), win, root: screen.root, gc, depth, a, keys, cursor_font, cursors: HashMap::new(), cursor_now: 0, clip_text: None, mapped: !hidden };
    // Started hidden (at login): stay in the tray if there is one, else show the window after a moment.
    let mut map_unless_tray = hidden.then(|| Instant::now() + Duration::from_secs(3));
    let ctx = egui::Context::default();
    let theme = if std::env::var("GTK_THEME").is_ok_and(|t| t.to_lowercase().contains("dark")) { egui::Theme::Dark } else { egui::Theme::Light };
    let mut raster = Raster::default();
    let mut bytes = Vec::new();
    let got = w_size(&c, win);
    let (mut width, mut height) = (if got.0 == 0 { init.0 } else { got.0 }, if got.1 == 0 { init.1 } else { got.1 });
    let mut events: Vec<Event> = vec![];
    let mut mods = Modifiers::NONE;
    let mut focused = true;
    let mut next_frame = Some(Instant::now());
    let mut dirty_put = false;
    let start = Instant::now();
    let shot = super::png::screenshot_request();
    let mut test_tray = u8::from(std::env::var_os("GANGLI_GUI_TEST_TRAY").is_some());
    let mut moved: Option<Instant> = None;

    'outer: loop {
        let wait = next_frame.map(|t| t.saturating_duration_since(Instant::now())).unwrap_or(Duration::from_secs(2)).min(Duration::from_secs(2));
        let first = match rx.recv_timeout(wait) {
            Ok(m) => Some(m),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let mut msgs: Vec<Msg> = first.into_iter().collect();
        while let Ok(m) = rx.try_recv() {
            msgs.push(m);
        }
        for m in msgs {
            match m {
                Msg::Gone(e) => {
                    super::glog(&format!("X server connection lost: {e}"));
                    break 'outer;
                }
                Msg::Wake => next_frame = Some(Instant::now()),
                Msg::Tray(a) => {
                    app.tray(a);
                    next_frame = Some(Instant::now());
                }
                Msg::X(e) => {
                    let to_pos = |x: i16, y: i16| Pos2::new(x as f32 / scale, y as f32 / scale);
                    match e {
                        XEvent::Expose(_) => dirty_put = true,
                        XEvent::ConfigureNotify(e) if e.window == win => {
                            // moved or resized: its place is read (and remembered) once it stays still
                            moved = Some(Instant::now());
                            if (e.width, e.height) != (width, height) {
                                width = e.width;
                                height = e.height;
                                next_frame = Some(Instant::now());
                            }
                        }
                        XEvent::MapNotify(_) => {
                            w.mapped = true;
                            next_frame = Some(Instant::now());
                        }
                        XEvent::UnmapNotify(_) => w.mapped = false,
                        XEvent::MotionNotify(e) => {
                            events.push(Event::PointerMoved(to_pos(e.event_x, e.event_y)));
                        }
                        XEvent::LeaveNotify(_) => events.push(Event::PointerGone),
                        XEvent::ButtonPress(e) | XEvent::ButtonRelease(e) => {
                            let pressed = e.response_type & 0x7f == x11rb::protocol::xproto::BUTTON_PRESS_EVENT;
                            let pointer = to_pos(e.event_x, e.event_y);
                            let m = modifiers(e.state.into());
                            let button = match e.detail {
                                1 => Some(PointerButton::Primary),
                                2 => Some(PointerButton::Middle),
                                3 => Some(PointerButton::Secondary),
                                8 => Some(PointerButton::Extra1),
                                9 => Some(PointerButton::Extra2),
                                _ => None,
                            };
                            if let Some(button) = button {
                                events.push(Event::PointerButton { pos: pointer, button, pressed, modifiers: m });
                            } else if pressed {
                                let delta = match e.detail {
                                    4 => egui::vec2(0.0, 1.0),
                                    5 => egui::vec2(0.0, -1.0),
                                    6 => egui::vec2(1.0, 0.0),
                                    7 => egui::vec2(-1.0, 0.0),
                                    _ => egui::Vec2::ZERO,
                                };
                                if delta != egui::Vec2::ZERO {
                                    events.push(Event::MouseWheel {
                                        unit: egui::MouseWheelUnit::Line,
                                        delta,
                                        phase: egui::TouchPhase::Move,
                                        source: egui::MouseWheelSource::Wheel,
                                        modifiers: m,
                                    });
                                }
                            }
                        }
                        XEvent::KeyPress(e) | XEvent::KeyRelease(e) => {
                            let pressed = e.response_type & 0x7f == x11rb::protocol::xproto::KEY_PRESS_EVENT;
                            let state: u16 = e.state.into();
                            let m = modifiers(state);
                            if m != mods {
                                mods = m;
                                events.push(Event::ModifiersChanged(m));
                            }
                            let sym = w.keys.lookup(e.detail, state);
                            // Modifier keys themselves: the state of the next event says it.
                            if (0xffe1..=0xffee).contains(&sym) || sym == 0xfe03 {
                                let after = match sym {
                                    0xffe1 | 0xffe2 => Some(KeyButMask::SHIFT),
                                    0xffe3 | 0xffe4 => Some(KeyButMask::CONTROL),
                                    0xffe9 | 0xffea => Some(KeyButMask::MOD1),
                                    _ => None,
                                };
                                if let Some(bit) = after {
                                    let s2 = if pressed { state | u16::from(bit) } else { state & !u16::from(bit) };
                                    let m2 = modifiers(s2);
                                    if m2 != mods {
                                        mods = m2;
                                        events.push(Event::ModifiersChanged(m2));
                                    }
                                }
                                continue;
                            }
                            let key = keysym_key(sym).or_else(|| {
                                // Ctrl + letter on any layout: the unshifted level.
                                keysym_key(w.keys.sym(e.detail, 0))
                            });
                            if pressed && m.ctrl && !m.alt {
                                match key {
                                    Some(Key::C) => events.push(Event::Copy),
                                    Some(Key::X) => events.push(Event::Cut),
                                    Some(Key::V) => w.request_paste(),
                                    _ => {}
                                }
                            }
                            if pressed && m.shift && key == Some(Key::Insert) {
                                w.request_paste();
                            }
                            if let Some(key) = key {
                                events.push(Event::Key { key, physical_key: None, pressed, repeat: false, modifiers: m });
                            }
                            if pressed && !m.ctrl && !m.alt {
                                if let Some(ch) = keysym_char(sym) {
                                    if !ch.is_control() {
                                        events.push(Event::Text(ch.to_string()));
                                    }
                                }
                            }
                        }
                        XEvent::FocusIn(_) => {
                            focused = true;
                            events.push(Event::WindowFocused(true));
                        }
                        XEvent::FocusOut(_) => {
                            focused = false;
                            events.push(Event::WindowFocused(false));
                        }
                        XEvent::ClientMessage(e) if e.type_ == w.a.protocols && e.data.as_data32()[0] == w.a.delete => {
                            if tray_ok.load(Ordering::SeqCst) {
                                w.hide();
                            } else {
                                app.tray(TrayAction::Quit);
                            }
                        }
                        XEvent::MappingNotify(_) => {
                            if let Ok(k) = keymap(&c) {
                                w.keys = k;
                            }
                        }
                        XEvent::SelectionRequest(e) => w.answer_selection(&e),
                        XEvent::SelectionClear(_) => w.clip_text = None,
                        XEvent::SelectionNotify(e) if e.property != x11rb::NONE => {
                            if let Ok(r) = c.get_property(true, win, e.property, AtomEnum::ANY, 0, 1 << 20).map_err(anyhow::Error::from).and_then(|c| Ok(c.reply()?)) {
                                let text = String::from_utf8_lossy(&r.value).to_string();
                                if !text.is_empty() {
                                    events.push(Event::Paste(text));
                                }
                            }
                        }
                        _ => {}
                    }
                    if !events.is_empty() {
                        next_frame = Some(Instant::now());
                    }
                }
            }
        }

        if test_tray == 1 && start.elapsed() >= Duration::from_secs(2) {
            test_tray = 2;
            let ev = ClientMessageEvent::new(32, win, w.a.protocols, [w.a.delete, 0, 0, 0, 0]);
            let _ = c.send_event(false, win, EventMask::NO_EVENT, ev);
            let _ = c.flush();
        } else if test_tray == 2 && start.elapsed() >= Duration::from_secs(5) {
            test_tray = 3;
            super::glog(&format!("test: window visible before reopening: {}", w.mapped));
            app.tray(TrayAction::Show);
        }
        if test_tray > 0 && test_tray < 3 {
            next_frame = Some(next_frame.map_or(Instant::now() + Duration::from_millis(300), |t| t.min(Instant::now() + Duration::from_millis(300))));
        }
        if let Some(t) = map_unless_tray {
            if tray_ok.load(Ordering::SeqCst) {
                map_unless_tray = None;
            } else if Instant::now() >= t {
                map_unless_tray = None;
                w.show();
            }
        }
        if moved.is_some_and(|t| t.elapsed() >= Duration::from_millis(600)) && w.mapped {
            moved = None;
            // The window's place on the screen (a window manager's frame makes ConfigureNotify's relative).
            if let Ok(Ok(r)) = c.translate_coordinates(win, screen.root, 0, 0).map(|ck| ck.reply()) {
                app.window_geometry(super::Geometry {
                    x: r.dst_x as f32 / scale,
                    y: r.dst_y as f32 / scale,
                    w: width as f32 / scale,
                    h: height as f32 / scale,
                });
            }
        }
        if moved.is_some() {
            next_frame = Some(next_frame.map_or(Instant::now() + Duration::from_millis(700), |t| t.min(Instant::now() + Duration::from_millis(700))));
        }
        app.logic(w.mapped);
        if app.take_show() {
            w.show();
        }
        if app.quit_requested() {
            break;
        }
        let m = app.tray_model();
        if m != model {
            let _ = tray_tx.send(m.clone());
            model = m;
        }
        if let Some((p, shot_after)) = &shot {
            if start.elapsed() >= *shot_after {
                let _ = std::fs::write(p, raster.png());
                super::glog(&format!("screenshot saved to {p}"));
                break;
            }
            next_frame = Some(next_frame.map_or(Instant::now() + Duration::from_millis(500), |t| t.min(Instant::now() + Duration::from_millis(500))));
        }

        let due = next_frame.is_some_and(|t| t <= Instant::now());
        if w.mapped && due {
            raster.resize(width as usize, height as usize);
            let mut raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(Pos2::ZERO, egui::vec2(width as f32 / scale, height as f32 / scale))),
                time: Some(start.elapsed().as_secs_f64()),
                focused,
                events: std::mem::take(&mut events),
                system_theme: Some(theme),
                ..Default::default()
            };
            raw.viewports.entry(ViewportId::ROOT).or_default().native_pixels_per_point = Some(scale);
            raw.viewports.entry(ViewportId::ROOT).or_default().focused = Some(focused);
            let out = ctx.run_ui(raw, |ui| app.ui(ui));
            raster.set_textures(&out.textures_delta);
            let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
            raster.paint(&prims, out.pixels_per_point, 0x202020);
            for id in &out.textures_delta.free {
                raster.free_texture(id);
            }
            if let Err(e) = w.put(&raster, &mut bytes) {
                super::glog(&format!("drawing failed: {e:#}"));
                break;
            }
            dirty_put = false;
            for cmd in out.platform_output.commands {
                match cmd {
                    egui::OutputCommand::CopyText(t) => w.copy(t),
                    egui::OutputCommand::OpenUrl(u) => {
                        let _ = std::process::Command::new("xdg-open").arg(u.url).spawn();
                    }
                    _ => {}
                }
            }
            if let Some(vo) = out.viewport_output.get(&ViewportId::ROOT) {
                if vo.commands.iter().any(|c| matches!(c, egui::ViewportCommand::RequestPaste)) {
                    w.request_paste();
                }
            }
            w.set_cursor(out.platform_output.cursor_icon);
            let delay = out.viewport_output.get(&ViewportId::ROOT).map(|v| v.repaint_delay).unwrap_or(Duration::MAX);
            next_frame = if delay < Duration::from_secs(3600) { Some(Instant::now() + delay) } else { None };
        } else if dirty_put && w.mapped {
            let _ = w.put(&raster, &mut bytes);
            dirty_put = false;
        } else if !w.mapped && due {
            next_frame = None;
        }
    }
    drop(tray_tx);
    app.shutdown();
    let _ = c.destroy_window(win);
    let _ = c.flush();
    Ok(())
}

fn w_size(c: &RustConnection, win: Window) -> (u16, u16) {
    c.get_geometry(win).ok().and_then(|g| g.reply().ok()).map(|g| (g.width, g.height)).unwrap_or((0, 0))
}
