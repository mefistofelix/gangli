//! The integrated terminal: a shell on a machine (or one of its persistent terminals) inside the gangli
//! window. Same resumable stream as the SSH gateway (the session survives connection drops), the screen kept
//! by alacritty_terminal, drawn with egui. Mouse selection copies, right click or Ctrl+V pastes, the wheel
//! scrolls back; the terminal's answers (cursor position, colours) go back to the program.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
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
    Exited(i32),
    Failed(String),
}

struct Screen {
    term: Term<Listener>,
    parser: Processor,
    state: State,
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
}

const BG: Color32 = Color32::from_rgb(24, 24, 27);
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
        let screen = Arc::new(Mutex::new(Screen { term, parser: Processor::new(), state: State::Connecting }));
        let (s2, w2) = (screen.clone(), wake.clone());
        let user = user.unwrap_or_else(|| "default".into());
        rt.spawn(async move {
            let fail = |e: String| {
                s2.lock().unwrap().state = State::Failed(e);
                w2();
            };
            let (net, _, node) = match crate::console::find_node(&c, &target) {
                Ok(x) => x,
                Err(e) => return fail(format!("{e:#}")),
            };
            // The size is known after the first frame; the first resize fixes it.
            let req = match persistent {
                Some(name) => Req::Attach { name, user, cols: 80, rows: 24 },
                None => Req::Shell { user, command: None, pty: Some((80, 24)), term: "xterm-256color".into() },
            };
            let (app, resp) = match crate::console::open_resumable(&c, &net, &node, req, Duration::from_secs(24 * 3600)).await {
                Ok(x) => x,
                Err(e) => return fail(format!("{e:#}")),
            };
            if !matches!(resp, Resp::Started) {
                return fail("unexpected answer".into());
            }
            s2.lock().unwrap().state = State::Running;
            w2();
            let (mut recv, mut send) = tokio::io::split(app);
            let writer = tokio::spawn(async move {
                while let Some(m) = rx.recv().await {
                    let r = match m {
                        ToNode::Data(d) => sframe::write(&mut send, sframe::DATA, &d).await,
                        ToNode::Resize(c, r) => sframe::write(&mut send, sframe::RESIZE, &sframe::resize(c, r)).await,
                    };
                    if r.is_err() {
                        break;
                    }
                }
                let _ = tokio::io::AsyncWriteExt::shutdown(&mut send).await;
            });
            let code = loop {
                match sframe::read(&mut recv).await {
                    Ok((sframe::DATA, body)) | Ok((sframe::STDERR, body)) => {
                        let mut g = s2.lock().unwrap();
                        let Screen { term, parser, .. } = &mut *g;
                        parser.advance(term, &body);
                        drop(g);
                        w2();
                    }
                    Ok((sframe::EXIT, body)) if body.len() >= 4 => break i32::from_be_bytes(body[..4].try_into().unwrap()),
                    Ok(_) => {}
                    Err(_) => break 255,
                }
            };
            writer.abort();
            s2.lock().unwrap().state = State::Exited(code);
            w2();
        });
        TermTab { label, screen, tx, title, size: (80, 24), id: egui::Id::unique(("term", rand_id())), scroll_rest: 0.0, focus_once: true }
    }

    pub fn title(&self) -> String {
        match self.title.lock().unwrap().clone() {
            Some(t) if !t.is_empty() => format!("{} · {t}", self.label),
            _ => self.label.clone(),
        }
    }

    pub fn ended(&self) -> bool {
        matches!(self.screen.lock().unwrap().state, State::Exited(_) | State::Failed(_))
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
        if resp.clicked() || resp.drag_started() || std::mem::take(&mut self.focus_once) {
            resp.request_focus();
        }
        let focused = resp.has_focus();
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

        // Keyboard.
        if focused {
            let mode = *g.term.mode();
            let app_cursor = mode.contains(TermMode::APP_CURSOR);
            let mut out: Vec<u8> = vec![];
            for e in ui.input(|i| i.events.clone()) {
                match e {
                    egui::Event::Text(t) => out.extend_from_slice(t.as_bytes()),
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
                    egui::Event::Key { key, pressed: true, modifiers, .. } => {
                        if let Some(b) = key_bytes(key, modifiers, app_cursor) {
                            out.extend_from_slice(&b);
                        }
                    }
                    _ => {}
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
        if show_cursor && cursor_row >= 0 && (cursor_row as usize) < rows as usize {
            let r = cell_rect(cursor_row as usize, cursor.column.0, 1);
            if focused {
                painter.rect_filled(r, 0.0, FG.gamma_multiply(0.55));
            } else {
                painter.rect_stroke(r, 0.0, egui::Stroke::new(1.0, FG), egui::StrokeKind::Inside);
            }
        }
        let note = match &g.state {
            State::Connecting => Some(super::tr::tr("connecting…", "collegamento…").to_string()),
            State::Exited(code) => Some(super::tr::trf(format!("[ended, exit code {code}]"), format!("[terminato, codice di uscita {code}]"))),
            State::Failed(e) => Some(e.clone()),
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
