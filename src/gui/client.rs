//! The GUI's console: one opened in this process, or the one already open in this folder (`gangli console
//! run`), reached through its local API. Every operation runs off the window thread; its answer comes back
//! on a channel and wakes the window.

use std::sync::{mpsc, Arc, Mutex};

use anyhow::Result;
use serde_json::Value;

use super::Waker;
use crate::console::Console;
use crate::ipc::IpcOp;

/// What an answer is for.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Tag {
    Status,
    Nodes,
    Tunnels,
    Ssh,
    Api,
    /// Persistent terminals of a machine (by node id).
    Terms(String),
    /// Network codes to show (a network was created, or its codes asked for).
    Codes,
    /// A new API token: shown in a dialog.
    Token,
    /// An API token asked for again: copy this field of the answer (`token` or `instructions`).
    CopyField(&'static str),
    /// An API token asked for again: show it to view and edit.
    TokenView,
    /// What a gangli executable carries: write it.
    Bundle,
    /// A launcher command line to start.
    Launch,
    /// Gateway login of a machine: copy the password.
    CopyPassword,
    /// Gateway login of a machine: copy an ssh command line.
    CopySsh,
    /// Any other action: on success, show this message.
    Done(String),
    /// Another `gangli` was started in this folder: bring the window up.
    Show,
    /// Housekeeping (the window's size and place saved): nothing to show, errors only logged.
    Quiet,
}

pub struct Reply {
    pub tag: Tag,
    pub result: Result<Value, String>,
}

enum Mode {
    Own { rt: tokio::runtime::Runtime, console: Arc<Console> },
    Other,
    /// The console could not start: the window says why.
    Broken(String),
}

pub struct Client {
    mode: Mode,
    tx: mpsc::Sender<Reply>,
    rx: mpsc::Receiver<Reply>,
    waker: Arc<Mutex<Option<Waker>>>,
}

fn wake(w: &Arc<Mutex<Option<Waker>>>) {
    if let Some(f) = w.lock().unwrap().as_ref() {
        f();
    }
}

impl Client {
    pub fn start() -> Result<Client> {
        let (tx, rx) = mpsc::channel();
        let waker: Arc<Mutex<Option<Waker>>> = Arc::default();
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
        let mode = match rt.block_on(crate::console::start(true)) {
            Ok(console) => {
                console.gui.store(true, std::sync::atomic::Ordering::SeqCst);
                let (c, tx2, w2) = (console.clone(), tx.clone(), waker.clone());
                rt.spawn(async move {
                    loop {
                        c.gui_show.notified().await;
                        let _ = tx2.send(Reply { tag: Tag::Show, result: Ok(Value::Null) });
                        wake(&w2);
                    }
                });
                Mode::Own { rt, console }
            }
            Err(e) => match crate::ipc::call_open(IpcOp::Status) {
                Ok(Some(_)) => Mode::Other,
                _ => Mode::Broken(format!("{e:#}")),
            },
        };
        Ok(Client { mode, tx, rx, waker })
    }

    pub fn set_waker(&self, w: Waker) {
        *self.waker.lock().unwrap() = Some(w);
    }

    /// True when the console lives in this window (closing gangli closes it).
    pub fn owns_console(&self) -> bool {
        matches!(self.mode, Mode::Own { .. })
    }

    /// The console of this window and its runtime (the integrated terminal needs them).
    pub fn console(&self) -> Option<(tokio::runtime::Handle, Arc<Console>)> {
        match &self.mode {
            Mode::Own { rt, console } => Some((rt.handle().clone(), console.clone())),
            _ => None,
        }
    }

    pub fn waker(&self) -> Waker {
        let w = self.waker.clone();
        Arc::new(move || wake(&w))
    }

    pub fn broken(&self) -> Option<&str> {
        match &self.mode {
            Mode::Broken(e) => Some(e),
            _ => None,
        }
    }

    pub fn try_recv(&self) -> Option<Reply> {
        self.rx.try_recv().ok()
    }

    pub fn call(&self, tag: Tag, op: IpcOp) {
        let (tx, w) = (self.tx.clone(), self.waker.clone());
        match &self.mode {
            Mode::Own { rt, console } => {
                let c = console.clone();
                rt.spawn(async move {
                    let result = crate::console::handle(&c, op).await.map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Reply { tag, result });
                    wake(&w);
                });
            }
            Mode::Other => {
                std::thread::spawn(move || {
                    let result = match crate::ipc::call_open(op) {
                        Ok(Some(v)) => Ok(v),
                        Ok(None) => Err("the console of this folder was closed".to_string()),
                        Err(e) => Err(format!("{e:#}")),
                    };
                    let _ = tx.send(Reply { tag, result });
                    wake(&w);
                });
            }
            Mode::Broken(e) => {
                let _ = tx.send(Reply { tag, result: Err(e.clone()) });
                wake(&w);
            }
        }
    }

    /// Close the console of this window (sessions, gateway, tunnels).
    pub fn shutdown(&mut self) {
        if let Mode::Own { rt, console } = std::mem::replace(&mut self.mode, Mode::Broken("closed".into())) {
            rt.block_on(async {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(3), console.shutdown()).await;
            });
            rt.shutdown_timeout(std::time::Duration::from_secs(1));
        }
    }
}
