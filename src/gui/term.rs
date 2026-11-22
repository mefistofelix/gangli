//! The integrated terminal: a shell on a machine (or one of its persistent terminals) inside the gangli
//! window. Same resumable stream as the SSH gateway (the session survives connection drops), the screen kept
//! by alacritty_terminal, drawn with egui. Mouse selection copies, right click or Ctrl+V pastes, the wheel
//! scrolls back; the terminal's answers (cursor position, colours) go back to the program.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::Scroll;
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb};
use egui::{Color32, FontId, Key, Pos2, Rect, Sense, Ui, Vec2};
use tokio::sync::mpsc;

use super::Waker;
use crate::console::Console;
use crate::proto::{sframe, Req, Resp};

enum ToNode {
    Data(Vec<u8>),
    Resize(u16, u16),
}

/// Receives what the terminal itself has to say: answers to the program's queries, the title.
#[derive(Clone)]
struct Listener {
    tx: mpsc::UnboundedSender<ToNode>,
    title: Arc<Mutex<Option<String>>>,
}

impl EventListener for Listener {
    fn send_event(&self, e: TermEvent) {
        match e {
            TermEvent::PtyWrite(s) => {
                let _ = self.tx.send(ToNode::Data(s.into_bytes()));
            }
            TermEvent::ColorRequest(i, answer) => {
                let c = palette(i);
                let _ = self.tx.send(ToNode::Data(answer(Rgb { r: c.r(), g: c.g(), b: c.b() }).into_bytes()));
            }
            TermEvent::Title(t) => *self.title.lock().unwrap() = Some(t),
            TermEvent::ResetTitle => *self.title.lock().unwrap() = None,
            _ => {}
        }
    }
}

#[derive(Clone, PartialEq)]
enum State {
    Connecting,
    Running,
    /// The session is gone with the connection: waiting for the machine to come back (then it goes on in
    /// this tab: a persistent terminal is reattached, else a new shell starts).
    Reconnecting(String),
    /// The shell ended: Enter starts a new one here.
    Exited(i32),
    /// The machine answered no (or the name is unknown): Enter tries again.
    Failed(String),
}

/// What a tab shows about its link: a colour for its dot, a short line and the details.
pub struct TabStatus {
    pub color: Color32,
    pub short: String,
    pub detail: String,
}

/// Bytes per second, short: `830 B/s`, `12 kB/s`, `3.4 MB/s`.
pub fn rate(bps: u64) -> String {
    match bps {
        0..=999 => format!("{bps} B/s"),
        1000..=99_999 => format!("{:.1} kB/s", bps as f64 / 1e3).replace(".0 ", " "),
        100_000..=999_999 => format!("{} kB/s", bps / 1000),
        _ => format!("{:.1} MB/s", bps as f64 / 1e6),
    }
}

struct Screen {
    term: Term<Listener>,
    parser: Processor,
    state: State,
    /// Counts the output received (predictions are dropped when it changes).
    output: u64,
}

pub struct TermTab {
    /// "user@machine" or "machine/name".
    pub label: String,
    screen: Arc<Mutex<Screen>>,
    tx: mpsc::UnboundedSender<ToNode>,
    title: Arc<Mutex<Option<String>>>,
    size: (u16, u16),
    id: egui::Id,
    scroll_rest: f32,
    focus_once: bool,
    /// Predictive echo: typed text shown at the cursor until the machine's echo arrives.
    predict: Predict,
    console: Arc<Console>,
    /// The machine's node id, once found.
    node: Arc<Mutex<Option<String>>>,
    /// Bytes of this terminal: received, sent; and the last rate sample (when, received, sent, rates).
    io: Arc<(AtomicU64, AtomicU64)>,
    sample: (std::time::Instant, u64, u64, u64, u64),
}

/// Typed while the machine is away: dropped, but the size is kept; `true` if Enter asks to retry now.
/// `None`: the tab was closed.
async fn idle(rx: &mut mpsc::UnboundedReceiver<ToNode>, size: &mut (u16, u16), wait: Option<Duration>) -> Option<bool> {
    let deadline = wait.map(|w| tokio::time::Instant::now() + w);
    loop {
        let m = match deadline {
            Some(d) => match tokio::time::timeout_at(d, rx.recv()).await {
                Err(_) => return Some(false),
                Ok(m) => m,
            },
            None => rx.recv().await,
        };
        match m? {
            ToNode::Resize(c, r) => *size = (c, r),
            ToNode::Data(d) if d.contains(&b'\r') => return Some(true),
            ToNode::Data(_) => {}
        }
    }
}

/// Write `bytes` on the screen as if the machine had sent them.
fn feed(screen: &Arc<Mutex<Screen>>, bytes: &[u8]) {
    let mut g = screen.lock().unwrap();
    let Screen { term, parser, output, .. } = &mut *g;
    parser.advance(term, bytes);
    *output += 1;
}

/// Local echo on slow links, the way mosh does it, simplified: predictions start on a line only once the
/// machine has echoed what was typed there (so a password prompt, which does not echo, never shows them),
/// and any output from the machine replaces them.
#[derive(Default)]
struct Predict {
    pending: String,
    /// The machine echoes on this line.
    line_echoes: bool,
    /// Cursor when text was last typed, and the output count then: echo detection.
    typed_at: Option<(i32, usize, u64)>,
    seen_output: u64,
    /// Shown only when the echo is late (fast links never see them).
    since: Option<std::time::Instant>,
    /// Where the predicted text starts (line, column): echoed characters are taken off its front.
    base: Option<(i32, usize)>,
}

pub(super) const BG: Color32 = Color32::from_rgb(24, 24, 27);
const FG: Color32 = Color32::from_rgb(220, 220, 220);

/// xterm's 256 colours (16 named, 6×6×6 cube, 24 greys) and the default foreground / background.
fn palette(i: usize) -> Color32 {
    const BASE: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 49, 49),
        (13, 188, 121),
        (229, 229, 16),
        (36, 114, 200),
        (188, 63, 188),
        (17, 168, 205),
        (229, 229, 229),
        (102, 102, 102),
        (241, 76, 76),
        (35, 209, 139),
        (245, 245, 67),
        (59, 142, 234),
        (214, 112, 214),
        (41, 184, 219),
        (255, 255, 255),
    ];
    match i {
        0..=15 => Color32::from_rgb(BASE[i].0, BASE[i].1, BASE[i].2),
        16..=231 => {
            let n = i - 16;
            let v = |x: usize| if x == 0 { 0 } else { (55 + x * 40) as u8 };
            Color32::from_rgb(v(n / 36), v((n / 6) % 6), v(n % 6))
        }
        232..=255 => {
            let g = (8 + (i - 232) * 10) as u8;
            Color32::from_rgb(g, g, g)
        }
        // NamedColor::Foreground = 256, Background = 257, Cursor = 258, then the dim ones.
        256 => FG,
        257 => BG,
        258 => FG,
        _ => FG,
    }
}

fn resolve(c: &Color, colors: &alacritty_terminal::term::color::Colors) -> Color32 {
    match c {
        Color::Spec(rgb) => Color32::from_rgb(rgb.r, rgb.g, rgb.b),
        Color::Indexed(i) => colors[*i as usize].map(|r| Color32::from_rgb(r.r, r.g, r.b)).unwrap_or_else(|| palette(*i as usize)),
        Color::Named(n) => {
            let i = *n as usize;
            let base = match n {
                NamedColor::Foreground | NamedColor::BrightForeground | NamedColor::Cursor => FG,
                NamedColor::DimForeground => FG.gamma_multiply(0.66),
                NamedColor::Background => BG,
                _ if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize).contains(&i) => {
                    palette(i - NamedColor::DimBlack as usize).gamma_multiply(0.66)
                }
                _ => palette(i),
            };
            colors[*n].map(|r| Color32::from_rgb(r.r, r.g, r.b)).unwrap_or(base)
        }
    }
}

impl TermTab {
    /// Open a shell on `target` (`name.network`) as `user`, or attach to its persistent terminal `persistent`.
    pub fn open(rt: &tokio::runtime::Handle, c: Arc<Console>, target: String, label: String, user: Option<String>, persistent: Option<String>, wake: Waker) -> TermTab {
        let (tx, mut rx) = mpsc::unbounded_channel::<ToNode>();
        let title = Arc::new(Mutex::new(None));
        let listener = Listener { tx: tx.clone(), title: title.clone() };
        let term = Term::new(Config::default(), &TermSize::new(80, 24), listener);
        let screen = Arc::new(Mutex::new(Screen { term, parser: Processor::new(), state: State::Connecting, output: 0 }));
        let (s2, w2) = (screen.clone(), wake.clone());
        let node_slot: Arc<Mutex<Option<String>>> = Arc::default();
        let io: Arc<(AtomicU64, AtomicU64)> = Arc::default();
        let (c2, n2, io2) = (c.clone(), node_slot.clone(), io.clone());
        let user = user.unwrap_or_else(|| "default".into());
        // One tab, as many sessions as it takes: when the connection is lost for good (the machine restarted,
        // was off too long), the tab waits for the machine and goes on here; after the shell ends, Enter
        // starts a new one.
        rt.spawn(async move {
            let c = c2;
            let set = |st: State| {
                s2.lock().unwrap().state = st;
                w2();
            };
            let (net, _, node) = match crate::console::find_node(&c, &target) {
                Ok(x) => x,
                Err(e) => return set(State::Failed(format!("{e:#}"))),
            };
            *n2.lock().unwrap() = Some(node.clone());
            let mut size = (80u16, 24u16);
            // Why the next session is not the first one (said on the screen when it starts).
            let mut why: Option<String> = None;
            loop {
                let req = match &persistent {
                    Some(name) => Req::Attach { name: name.clone(), user: user.clone(), cols: size.0, rows: size.1 },
                    None => Req::Shell { user: user.clone(), command: None, pty: Some(size), term: "xterm-256color".into() },
                };
                let app = match crate::console::open_resumable(&c, &net, &node, req, Duration::from_secs(24 * 3600)).await {
                    Ok((app, Resp::Started)) => app,
                    Ok(_) => {
                        set(State::Failed("unexpected answer from the machine".into()));
                        if idle(&mut rx, &mut size, None).await.is_none() {
                            return;
                        }
                        continue;
                    }
                    Err(e) if c.link(&node).is_some() => {
                        // The machine is there and said no (an unknown user, say).
                        set(State::Failed(format!("{e:#}")));
                        if idle(&mut rx, &mut size, None).await.is_none() {
                            return;
                        }
                        continue;
                    }
                    Err(_) => {
                        set(State::Reconnecting("the machine is not reachable: waiting for it".into()));
                        if idle(&mut rx, &mut size, Some(Duration::from_secs(3))).await.is_none() {
                            return;
                        }
                        continue;
                    }
                };
                if let Some(w) = why.take() {
                    let what = match &persistent {
                        Some(n) => format!("{w}: reattached to the persistent terminal {n}"),
                        None => format!("{w}: a new shell"),
                    };
                    feed(&s2, format!("\r\n\x1b[2m— {what} —\x1b[0m\r\n").as_bytes());
                }
                set(State::Running);
                let (mut recv, mut send) = tokio::io::split(app);
                let _ = sframe::write(&mut send, sframe::RESIZE, &sframe::resize(size.0, size.1)).await;
                // The reader runs apart: a frame half read must never be cut by the select below.
                let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
                let reader = tokio::spawn(async move {
                    loop {
                        let r = sframe::read(&mut recv).await.map_err(|_| ());
                        let stop = r.is_err();
                        if ev_tx.send(r).is_err() || stop {
                            break;
                        }
                    }
                });
                let exit = loop {
                    tokio::select! {
                        m = rx.recv() => {
                            let Some(m) = m else {
                                reader.abort();
                                return;
                            };
                            let r = match m {
                                ToNode::Data(d) => {
                                    io2.1.fetch_add(d.len() as u64, Ordering::Relaxed);
                                    sframe::write(&mut send, sframe::DATA, &d).await
                                }
                                ToNode::Resize(cc, rr) => {
                                    size = (cc, rr);
                                    sframe::write(&mut send, sframe::RESIZE, &sframe::resize(cc, rr)).await
                                }
                            };
                            if r.is_err() {
                                break None;
                            }
                        }
                        e = ev_rx.recv() => match e {
                            Some(Ok((sframe::DATA, body))) | Some(Ok((sframe::STDERR, body))) => {
                                io2.0.fetch_add(body.len() as u64, Ordering::Relaxed);
                                feed(&s2, &body);
                                w2();
                            }
                            Some(Ok((sframe::EXIT, body))) if body.len() >= 4 => break Some(i32::from_be_bytes(body[..4].try_into().unwrap())),
                            Some(Ok(_)) => {}
                            _ => break None,
                        }
                    }
                };
                reader.abort();
                let _ = tokio::io::AsyncWriteExt::shutdown(&mut send).await;
                match exit {
                    Some(code) => {
                        set(State::Exited(code));
                        if idle(&mut rx, &mut size, None).await.is_none() {
                            return;
                        }
                        why = Some("ended".into());
                    }
                    None => why = Some("connection lost".into()),
                }
            }
        });
        TermTab {
            label,
            screen,
            tx,
            title,
            size: (80, 24),
            id: egui::Id::unique(("term", rand_id())),
            scroll_rest: 0.0,
            focus_once: true,
            predict: Predict::default(),
            console: c,
            node: node_slot,
            io,
            sample: (std::time::Instant::now(), 0, 0, 0, 0),
        }
    }

    /// The tab's link: its dot's colour, a short line, the details (session, path, latency, traffic).
    pub fn status(&mut self) -> TabStatus {
        let (rx, tx) = (self.io.0.load(Ordering::Relaxed), self.io.1.load(Ordering::Relaxed));
        let dt = self.sample.0.elapsed().as_secs_f64();
        if dt >= 1.0 {
            let s = self.sample;
            self.sample = (std::time::Instant::now(), rx, tx, (rx.saturating_sub(s.1) as f64 / dt) as u64, (tx.saturating_sub(s.2) as f64 / dt) as u64);
        }
        let link = self.node.lock().unwrap().clone().and_then(|n| self.console.link(&n));
        let state = self.screen.lock().unwrap().state.clone();
        let amber = Color32::from_rgb(230, 170, 60);
        let (color, short) = match (&state, &link) {
            (State::Running, Some(p)) => (
                if p.relay { amber } else { Color32::from_rgb(60, 180, 100) },
                format!("{} · {} ms", if p.relay { "relay" } else { "direct" }, p.rtt_ms),
            ),
            (State::Running, None) => (amber, "reconnecting…".to_string()),
            (State::Connecting, _) => (amber, "connecting…".to_string()),
            (State::Reconnecting(_), _) => (amber, "waiting for the machine".to_string()),
            (State::Exited(code), _) => (Color32::GRAY, format!("ended ({code})")),
            (State::Failed(_), _) => (Color32::from_rgb(220, 80, 80), "failed".to_string()),
        };
        let mut detail = match &state {
            State::Reconnecting(w) | State::Failed(w) => w.clone(),
            _ => short.clone(),
        };
        if let Some(p) = &link {
            detail += &format!(
                "\n{} {} ms{}\nthis terminal: ↓ {} ↑ {}\nthe whole connection: ↓ {} ↑ {}",
                if p.relay { "through a relay," } else { "direct," },
                p.rtt_ms,
                if p.remote.is_empty() { String::new() } else { format!(", {}", p.remote) },
                rate(self.sample.3),
                rate(self.sample.4),
                rate(p.rx_bps),
                rate(p.tx_bps)
            );
        }
        TabStatus { color, short, detail }
    }

    /// The traffic of this terminal now: received, sent (bytes per second).
    pub fn traffic(&self) -> (u64, u64) {
        (self.sample.3, self.sample.4)
    }

    pub fn title(&self) -> String {
        match self.title.lock().unwrap().clone() {
            Some(t) if !t.is_empty() => format!("{} · {t}", self.label),
            _ => self.label.clone(),
        }
    }

    pub fn ended(&self) -> bool {
        matches!(self.screen.lock().unwrap().state, State::Exited(_))
    }

    fn send(&self, d: impl Into<Vec<u8>>) {
        let _ = self.tx.send(ToNode::Data(d.into()));
    }

    pub fn ui(&mut self, ui: &mut Ui) {
        let font = FontId::monospace(14.0);
        let (cw, ch) = ui.ctx().fonts_mut(|f| (f.glyph_width(&font, 'M'), f.row_height(&font)));
        let avail = ui.available_size();
        let (_, rect) = ui.allocate_space(avail);
        let resp = ui.interact(rect, self.id, Sense::click_and_drag());
        let cols = ((rect.width() / cw).floor() as u16).max(10);
        let rows = ((rect.height() / ch).floor() as u16).max(3);
        if (cols, rows) != self.size {
            self.size = (cols, rows);
            self.screen.lock().unwrap().term.resize(TermSize::new(cols as usize, rows as usize));
            let _ = self.tx.send(ToNode::Resize(cols, rows));
        }
        if resp.clicked() || resp.drag_started() || self.focus_once {
            resp.request_focus();
        }
        let focused = resp.has_focus();
        if focused {
            self.focus_once = false;
        }
        if focused {
            ui.memory_mut(|m| m.set_focus_lock_filter(self.id, egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true }));
        }

        let mut g = self.screen.lock().unwrap();
        // Mouse: wheel scrolls back, drag selects (and copies), right click pastes.
        let cell_at = |p: Pos2, offset: usize| {
            let col = (((p.x - rect.min.x) / cw).max(0.0) as usize).min(cols as usize - 1);
            let row = (((p.y - rect.min.y) / ch).max(0.0) as usize).min(rows as usize - 1);
            let side = if (p.x - rect.min.x) / cw - col as f32 > 0.5 { Side::Right } else { Side::Left };
            (Point::new(Line(row as i32 - offset as i32), Column(col)), side)
        };
        if resp.hovered() {
            let dy = ui.input(|i| i.smooth_scroll_delta.y);
            if dy != 0.0 {
                self.scroll_rest += dy / ch;
                let lines = self.scroll_rest.trunc() as i32;
                if lines != 0 {
                    self.scroll_rest -= lines as f32;
                    g.term.scroll_display(Scroll::Delta(lines));
                }
            }
        }
        let offset = g.term.grid().display_offset();
        if let Some(p) = resp.interact_pointer_pos() {
            let (pt, side) = cell_at(p, offset);
            if resp.drag_started() {
                g.term.selection = Some(Selection::new(SelectionType::Simple, pt, side));
            } else if resp.dragged() {
                if let Some(s) = g.term.selection.as_mut() {
                    s.update(pt, side);
                }
            }
        }
        if resp.drag_stopped() {
            if let Some(t) = g.term.selection_to_string().filter(|t| !t.is_empty()) {
                ui.ctx().copy_text(t);
            }
        }
        if resp.clicked() {
            g.term.selection = None;
        }
        if resp.secondary_clicked() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::RequestPaste);
        }

        // Output from the machine replaces the predictions; did it echo what was typed?
        if g.output != self.predict.seen_output {
            self.predict.seen_output = g.output;
            let c = g.term.grid().cursor.point;
            match self.predict.base {
                // Echo arrived for part of it: keep the rest.
                Some((line, col)) if c.line.0 == line && c.column.0 >= col => {
                    let echoed = c.column.0 - col;
                    self.predict.pending = self.predict.pending.chars().skip(echoed).collect();
                    self.predict.base = Some((line, c.column.0));
                }
                _ => self.predict.pending.clear(),
            }
            if self.predict.pending.is_empty() {
                self.predict.base = None;
            }
            if let Some((line, col, _)) = self.predict.typed_at.take() {
                let c = g.term.grid().cursor.point;
                if c.line.0 == line && c.column.0 > col {
                    self.predict.line_echoes = true;
                }
            }
        }

        // Keyboard.
        if focused {
            let mode = *g.term.mode();
            let app_cursor = mode.contains(TermMode::APP_CURSOR);
            let mut out: Vec<u8> = vec![];
            let mut typed = String::new();
            for e in ui.input(|i| i.events.clone()) {
                match e {
                    egui::Event::Text(t) => {
                        out.extend_from_slice(t.as_bytes());
                        typed.push_str(&t);
                    }
                    egui::Event::Key { key: Key::Backspace, pressed: true, modifiers, .. } if !modifiers.ctrl && !self.predict.pending.is_empty() => {
                        self.predict.pending.pop();
                        out.extend_from_slice(b"\x7f");
                    }
                    egui::Event::Key { key, pressed: true, modifiers, .. } if key_bytes(key, modifiers, app_cursor).is_some() => {
                        // Anything but plain typing: no prediction until the machine answers; a new line (or
                        // Ctrl / Esc) may be a prompt that does not echo.
                        self.predict.pending.clear();
                        if matches!(key, Key::Enter | Key::Escape) || modifiers.ctrl {
                            self.predict.line_echoes = false;
                        }
                        out.extend_from_slice(&key_bytes(key, modifiers, app_cursor).unwrap_or_default());
                    }
                    egui::Event::Paste(t) => {
                        let t = t.replace("\r\n", "\r").replace('\n', "\r");
                        if mode.contains(TermMode::BRACKETED_PASTE) {
                            out.extend_from_slice(b"\x1b[200~");
                            out.extend_from_slice(t.as_bytes());
                            out.extend_from_slice(b"\x1b[201~");
                        } else {
                            out.extend_from_slice(t.as_bytes());
                        }
                    }
                    _ => {}
                }
            }
            if !typed.is_empty() {
                if self.predict.line_echoes && !mode.contains(TermMode::ALT_SCREEN) {
                    if self.predict.pending.is_empty() {
                        let c = g.term.grid().cursor.point;
                        self.predict.base = Some((c.line.0, c.column.0));
                    }
                    self.predict.pending.push_str(&typed);
                }
                if self.predict.typed_at.is_none() {
                    let c = g.term.grid().cursor.point;
                    self.predict.typed_at = Some((c.line.0, c.column.0, g.output));
                }
            }
            if !out.is_empty() {
                g.term.selection = None;
                g.term.scroll_display(Scroll::Bottom);
                drop(g);
                self.send(out);
                g = self.screen.lock().unwrap();
            }
        }

        // Screen.
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, BG);
        let content = g.term.renderable_content();
        let offset = content.display_offset;
        let colors = content.colors;
        let selection = content.selection;
        let cursor = content.cursor.point;
        let show_cursor = content.mode.contains(TermMode::SHOW_CURSOR);
        let cell_rect = |row: usize, col: usize, w: usize| Rect::from_min_size(rect.min + Vec2::new(col as f32 * cw, row as f32 * ch), Vec2::new(cw * w as f32, ch));
        // Runs of ASCII text with one look, painted at once; other characters one by one at their column.
        let mut run = String::new();
        let mut run_at = (0usize, 0usize, FG, false, false);
        let flush = |run: &mut String, at: (usize, usize, Color32, bool, bool)| {
            if run.is_empty() {
                return;
            }
            let mut job = egui::text::LayoutJob::default();
            let mut fmt = egui::TextFormat { font_id: font.clone(), color: at.2, italics: at.3, ..Default::default() };
            if at.4 {
                fmt.underline = egui::Stroke::new(1.0, at.2);
            }
            job.append(run, 0.0, fmt);
            let galley = ui.ctx().fonts_mut(|f| f.layout_job(job));
            painter.galley(cell_rect(at.0, at.1, 1).min, galley, at.2);
            run.clear();
        };
        for cell in content.display_iter {
            let row = (cell.point.line.0 + offset as i32) as usize;
            let col = cell.point.column.0;
            if row >= rows as usize || cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let mut fg = resolve(&cell.fg, colors);
            let mut bg = resolve(&cell.bg, colors);
            if cell.flags.contains(Flags::BOLD) {
                if let Color::Named(n) = cell.fg {
                    if (n as usize) < 8 {
                        fg = palette(n as usize + 8);
                    }
                }
            }
            if cell.flags.contains(Flags::DIM) {
                fg = fg.gamma_multiply(0.66);
            }
            if cell.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if selection.is_some_and(|s| s.contains(cell.point)) {
                bg = Color32::from_rgb(60, 90, 140);
            }
            if cell.flags.contains(Flags::HIDDEN) {
                fg = bg;
            }
            let wide = if cell.flags.contains(Flags::WIDE_CHAR) { 2 } else { 1 };
            if bg != BG {
                painter.rect_filled(cell_rect(row, col, wide), 0.0, bg);
            }
            let italic = cell.flags.contains(Flags::ITALIC);
            let under = cell.flags.intersects(Flags::ALL_UNDERLINES);
            let ch_ = cell.c;
            let look = (fg, italic, under);
            let continues = !run.is_empty() && run_at.0 == row && run_at.1 + run.chars().count() == col && (run_at.2, run_at.3, run_at.4) == look;
            if ch_.is_ascii() && continues {
                run.push(ch_);
                continue;
            }
            flush(&mut run, run_at);
            if ch_.is_ascii() {
                run.push(ch_);
                run_at = (row, col, fg, italic, under);
            } else if ch_ != ' ' {
                let mut s = String::from(ch_);
                if let Some(extra) = cell.zerowidth() {
                    s.extend(extra.iter());
                }
                run_at = (row, col, fg, italic, under);
                let mut one = s;
                flush(&mut one, run_at);
            }
        }
        flush(&mut run, run_at);
        let cursor_row = cursor.line.0 + offset as i32;
        let mut cursor_col = cursor.column.0;
        // Predicted echo: typed text not yet echoed by the machine, underlined, at the cursor, once the echo is late.
        if self.predict.pending.is_empty() {
            self.predict.since = None;
        } else if self.predict.since.is_none() {
            self.predict.since = Some(std::time::Instant::now());
            ui.ctx().request_repaint_after(Duration::from_millis(45));
        }
        let late = self.predict.since.is_some_and(|t| t.elapsed() >= Duration::from_millis(40));
        if late && offset == 0 && cursor_row >= 0 && (cursor_row as usize) < rows as usize {
            let color = FG.gamma_multiply(0.8);
            for ch_ in self.predict.pending.chars() {
                if cursor_col >= cols as usize {
                    break;
                }
                let r = cell_rect(cursor_row as usize, cursor_col, 1);
                painter.rect_filled(r, 0.0, BG);
                let mut job = egui::text::LayoutJob::default();
                job.append(&ch_.to_string(), 0.0, egui::TextFormat { font_id: font.clone(), color, underline: egui::Stroke::new(1.0, color), ..Default::default() });
                let galley = ui.ctx().fonts_mut(|f| f.layout_job(job));
                painter.galley(r.min, galley, color);
                cursor_col += 1;
            }
        }
        if show_cursor && cursor_row >= 0 && (cursor_row as usize) < rows as usize {
            let r = cell_rect(cursor_row as usize, cursor_col.min(cols as usize - 1), 1);
            if focused {
                painter.rect_filled(r, 0.0, FG.gamma_multiply(0.55));
            } else {
                painter.rect_stroke(r, 0.0, egui::Stroke::new(1.0, FG), egui::StrokeKind::Inside);
            }
        }
        let note = match &g.state {
            State::Connecting => Some("connecting…".to_string()),
            State::Reconnecting(w) => Some(format!("{w}: it goes on here when it is back (Enter: try now)")),
            State::Exited(code) => Some(format!("[ended, exit code {code}] Enter: a new shell here")),
            State::Failed(e) => Some(format!("{e} — Enter: try again")),
            State::Running => None,
        };
        if let Some(n) = note {
            painter.text(rect.right_bottom() - Vec2::new(8.0, 6.0), egui::Align2::RIGHT_BOTTOM, n, FontId::proportional(13.0), Color32::from_rgb(230, 170, 60));
        }
    }
}

fn rand_id() -> u64 {
    u64::from_le_bytes(crate::util::random16()[..8].try_into().unwrap())
}

/// The bytes a key sends (xterm conventions).
fn key_bytes(key: Key, m: egui::Modifiers, app_cursor: bool) -> Option<Vec<u8>> {
    let csi_mod = |base: &str, fin: char| -> Vec<u8> {
        let n = 1 + (m.shift as u8) + 2 * (m.alt as u8) + 4 * (m.ctrl as u8);
        if n > 1 {
            format!("\x1b[1;{n}{fin}").into_bytes()
        } else {
            format!("{base}{fin}").into_bytes()
        }
    };
    let tilde = |code: u8| -> Vec<u8> {
        let n = 1 + (m.shift as u8) + 2 * (m.alt as u8) + 4 * (m.ctrl as u8);
        if n > 1 {
            format!("\x1b[{code};{n}~").into_bytes()
        } else {
            format!("\x1b[{code}~").into_bytes()
        }
    };
    let arrow_base = if app_cursor { "\x1bO" } else { "\x1b[" };
    Some(match key {
        Key::Enter => b"\r".to_vec(),
        Key::Backspace => if m.ctrl { b"\x08".to_vec() } else { b"\x7f".to_vec() },
        Key::Tab => if m.shift { b"\x1b[Z".to_vec() } else { b"\t".to_vec() },
        Key::Escape => b"\x1b".to_vec(),
        Key::ArrowUp => csi_mod(arrow_base, 'A'),
        Key::ArrowDown => csi_mod(arrow_base, 'B'),
        Key::ArrowRight => csi_mod(arrow_base, 'C'),
        Key::ArrowLeft => csi_mod(arrow_base, 'D'),
        Key::Home => csi_mod(arrow_base, 'H'),
        Key::End => csi_mod(arrow_base, 'F'),
        Key::Insert => tilde(2),
        Key::Delete => tilde(3),
        Key::PageUp => tilde(5),
        Key::PageDown => tilde(6),
        Key::F1 => csi_mod("\x1bO", 'P'),
        Key::F2 => csi_mod("\x1bO", 'Q'),
        Key::F3 => csi_mod("\x1bO", 'R'),
        Key::F4 => csi_mod("\x1bO", 'S'),
        Key::F5 => tilde(15),
        Key::F6 => tilde(17),
        Key::F7 => tilde(18),
        Key::F8 => tilde(19),
        Key::F9 => tilde(20),
        Key::F10 => tilde(21),
        Key::F11 => tilde(23),
        Key::F12 => tilde(24),
        // Ctrl + letter: the control character (Ctrl+V pastes instead, through the Paste event).
        k if m.ctrl && !m.alt && k != Key::V => {
            let name = k.name();
            let c = name.chars().next()?;
            if name.len() == 1 && c.is_ascii_alphabetic() {
                vec![(c.to_ascii_uppercase() as u8) & 0x1f]
            } else {
                match k {
                    Key::Space => vec![0],
                    Key::OpenBracket => vec![0x1b],
                    Key::Backslash => vec![0x1c],
                    Key::CloseBracket => vec![0x1d],
                    _ => return None,
                }
            }
        }
        // Alt + letter: ESC then the letter.
        k if m.alt && !m.ctrl => {
            let name = k.name();
            let c = name.chars().next()?;
            if name.len() == 1 && c.is_ascii_alphanumeric() {
                let c = if m.shift { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() };
                vec![0x1b, c as u8]
            } else {
                return None;
            }
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_palette() {
        let none = egui::Modifiers::NONE;
        assert_eq!(key_bytes(Key::ArrowUp, none, false).unwrap(), b"\x1b[A");
        assert_eq!(key_bytes(Key::ArrowUp, none, true).unwrap(), b"\x1bOA");
        assert_eq!(key_bytes(Key::C, egui::Modifiers::CTRL, false).unwrap(), vec![3]);
        assert_eq!(key_bytes(Key::ArrowLeft, egui::Modifiers::CTRL, false).unwrap(), b"\x1b[1;5D");
        assert_eq!(key_bytes(Key::Delete, none, false).unwrap(), b"\x1b[3~");
        assert_eq!(palette(196), Color32::from_rgb(255, 0, 0));
        assert_eq!(palette(244), Color32::from_rgb(128, 128, 128));
    }
}
