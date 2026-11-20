//! Persistent terminals on the node (section 6 of docs/ARCHITECTURE.md): a named shell whose PTY lives on
//! the node, detached from any connection, like a tmux session. A VT engine (alacritty_terminal) keeps the
//! screen; whoever attaches gets the screen redrawn exactly, then the live output.
//!
//! On slow links the output is not queued without end: each viewer has a small queue, and when it is full
//! the intermediate output is dropped and the viewer gets the current screen instead (state sync, as mosh
//! does), so a `cat bigfile` on a mobile link costs one screen, not the whole file.
//!
//! Reached through the SSH gateway with `user+machine+name` (any SSH client), and by the GUI.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, Processor};
use anyhow::{anyhow, Result};
use tokio::sync::mpsc;

use crate::proc::{In, Out};

/// Chunks a viewer may lag behind before it gets a redraw instead.
const VIEWER_QUEUE: usize = 64;

pub struct Viewer {
    pub rx: mpsc::Receiver<Vec<u8>>,
    /// Set when output was dropped for this viewer: it needs a redraw.
    pub dirty: Arc<AtomicBool>,
    pub exited: Arc<AtomicBool>,
}

struct ViewerTx {
    tx: mpsc::Sender<Vec<u8>>,
    dirty: Arc<AtomicBool>,
    exited: Arc<AtomicBool>,
}

pub struct Session {
    pub name: String,
    pub user: String,
    pub started: u64,
    /// Dropping it (kill) ends the shell.
    input: Mutex<Option<std::sync::mpsc::Sender<In>>>,
    screen: Mutex<(Term<VoidListener>, Processor)>,
    viewers: Mutex<Vec<ViewerTx>>,
    pub exit: Mutex<Option<i32>>,
}

type Map = Mutex<HashMap<String, Arc<Session>>>;

fn sessions() -> &'static Map {
    static S: OnceLock<Map> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// The persistent terminal `name`, started now (as `user`, `cols`×`rows`) if it does not exist.
pub fn get_or_start(name: &str, user: &str, cols: u16, rows: u16) -> Result<Arc<Session>> {
    if let Some(s) = sessions().lock().unwrap().get(name) {
        return Ok(s.clone());
    }
    let spec = crate::proc::Spec { user: user.to_string(), command: None, pty: Some((cols, rows)), term: "xterm-256color".into() };
    let child = crate::proc::spawn(&spec)?;
    let size = TermSize::new(cols as usize, rows as usize);
    let term = Term::new(Config::default(), &size, VoidListener);
    let s = Arc::new(Session {
        name: name.to_string(),
        user: user.to_string(),
        started: crate::util::now(),
        input: Mutex::new(Some(child.input)),
        screen: Mutex::new((term, Processor::new())),
        viewers: Mutex::default(),
        exit: Mutex::new(None),
    });
    sessions().lock().unwrap().insert(name.to_string(), s.clone());
    let (s2, mut output) = (s.clone(), child.output);
    tokio::spawn(async move {
        while let Some(o) = output.recv().await {
            match o {
                Out::Stdout(d) | Out::Stderr(d) => s2.feed(d),
                Out::Exit(code) => {
                    *s2.exit.lock().unwrap() = Some(code);
                    for v in s2.viewers.lock().unwrap().drain(..) {
                        v.exited.store(true, Ordering::SeqCst);
                    }
                    break;
                }
            }
        }
        sessions().lock().unwrap().remove(&s2.name);
    });
    Ok(s)
}

impl Session {
    fn feed(&self, d: Vec<u8>) {
        // Screen and viewer queues change together, so a redraw never misses or repeats output.
        let mut g = self.screen.lock().unwrap();
        let (term, parser) = &mut *g;
        parser.advance(term, &d);
        let mut viewers = self.viewers.lock().unwrap();
        viewers.retain(|v| !v.tx.is_closed());
        for v in viewers.iter() {
            if v.dirty.load(Ordering::SeqCst) {
                continue; // it will get a redraw anyway
            }
            if v.tx.try_send(d.clone()).is_err() {
                v.dirty.store(true, Ordering::SeqCst);
            }
        }
    }

    /// A new viewer; its first bytes are a full redraw of the current screen.
    pub fn attach(&self) -> Viewer {
        let (tx, rx) = mpsc::channel(VIEWER_QUEUE);
        let dirty = Arc::new(AtomicBool::new(true));
        let exited = Arc::new(AtomicBool::new(false));
        self.viewers.lock().unwrap().push(ViewerTx { tx, dirty: dirty.clone(), exited: exited.clone() });
        Viewer { rx, dirty, exited }
    }

    pub fn input(&self, d: Vec<u8>) {
        if let Some(i) = self.input.lock().unwrap().as_ref() {
            let _ = i.send(In::Data(d));
        }
    }

    /// The last viewer to attach or resize sets the size.
    pub fn resize(&self, cols: u16, rows: u16) {
        if let Some(i) = self.input.lock().unwrap().as_ref() {
            let _ = i.send(In::Resize(cols, rows));
        }
        let mut g = self.screen.lock().unwrap();
        g.0.resize(TermSize::new(cols as usize, rows as usize));
    }

    /// The whole screen as escape sequences (clear, every cell with its attributes, cursor, modes), after
    /// dropping what the viewer had queued: the redraw already contains it.
    pub fn redraw(&self, queued: &mut mpsc::Receiver<Vec<u8>>) -> Vec<u8> {
        let g = self.screen.lock().unwrap();
        while queued.try_recv().is_ok() {}
        render(&g.0)
    }
}

pub fn list() -> Vec<(String, String, u64, usize)> {
    let all = sessions().lock().unwrap();
    let mut v: Vec<_> = all.values().map(|s| (s.name.clone(), s.user.clone(), s.started, s.viewers.lock().unwrap().len())).collect();
    v.sort();
    v
}

pub fn kill(name: &str) -> Result<()> {
    let s = sessions().lock().unwrap().remove(name).ok_or_else(|| anyhow!("no terminal {name}"))?;
    s.input.lock().unwrap().take();
    Ok(())
}

fn sgr_color(out: &mut String, c: &Color, fg: bool) {
    match c {
        Color::Named(n) => {
            let i = *n as usize;
            if i < 8 {
                out.push_str(&format!(";{}", if fg { 30 } else { 40 } + i));
            } else if i < 16 {
                out.push_str(&format!(";{}", if fg { 90 } else { 100 } + i - 8));
            } else {
                out.push_str(if fg { ";39" } else { ";49" });
            }
        }
        Color::Indexed(i) => out.push_str(&format!(";{};5;{i}", if fg { 38 } else { 48 })),
        Color::Spec(rgb) => out.push_str(&format!(";{};2;{};{};{}", if fg { 38 } else { 48 }, rgb.r, rgb.g, rgb.b)),
    }
}

fn render(term: &Term<VoidListener>) -> Vec<u8> {
    let mut out = String::from("\x1b[0m\x1b[H\x1b[2J");
    let grid = term.grid();
    let (cols, rows) = (grid.columns(), grid.screen_lines());
    let mut last = String::new();
    for row in 0..rows {
        out.push_str(&format!("\x1b[{};1H", row + 1));
        let line = &grid[alacritty_terminal::index::Line(row as i32)];
        for col in 0..cols {
            let cell = &line[alacritty_terminal::index::Column(col)];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let mut sgr = String::from("\x1b[0");
            if cell.flags.contains(Flags::BOLD) {
                sgr.push_str(";1");
            }
            if cell.flags.contains(Flags::DIM) {
                sgr.push_str(";2");
            }
            if cell.flags.contains(Flags::ITALIC) {
                sgr.push_str(";3");
            }
            if cell.flags.intersects(Flags::ALL_UNDERLINES) {
                sgr.push_str(";4");
            }
            if cell.flags.contains(Flags::INVERSE) {
                sgr.push_str(";7");
            }
            if cell.flags.contains(Flags::HIDDEN) {
                sgr.push_str(";8");
            }
            if cell.flags.contains(Flags::STRIKEOUT) {
                sgr.push_str(";9");
            }
            sgr_color(&mut sgr, &cell.fg, true);
            sgr_color(&mut sgr, &cell.bg, false);
            sgr.push('m');
            if sgr != last {
                out.push_str(&sgr);
                last = sgr;
            }
            out.push(cell.c);
            if let Some(extra) = cell.zerowidth() {
                out.extend(extra.iter());
            }
        }
    }
    out.push_str("\x1b[0m");
    let cur = grid.cursor.point;
    out.push_str(&format!("\x1b[{};{}H", cur.line.0 + 1, cur.column.0 + 1));
    let mode = term.mode();
    out.push_str(if mode.contains(TermMode::SHOW_CURSOR) { "\x1b[?25h" } else { "\x1b[?25l" });
    out.push_str(if mode.contains(TermMode::APP_CURSOR) { "\x1b[?1h" } else { "\x1b[?1l" });
    out.push_str(if mode.contains(TermMode::BRACKETED_PASTE) { "\x1b[?2004h" } else { "\x1b[?2004l" });
    out.into_bytes()
}
