//! The window content, the same on every OS: machines, networks, tunnels, SSH logins, API tokens and this
//! computer's node. Everything goes through the console (`client.rs`); nothing here talks to the network.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{Align, Color32, Layout, RichText, Ui};
use serde_json::Value;

use super::client::{Client, Tag};
use super::tr::{italian, set_italian, tr, trf};
use super::{TrayAction, TrayModel};
use crate::ipc::{IpcOp, NodeView};
use crate::util::{human_ago, human_bytes, now};

const REFRESH: Duration = Duration::from_secs(2);
const REFRESH_HIDDEN: Duration = Duration::from_secs(8);
const TOAST: Duration = Duration::from_secs(6);

const GREEN: Color32 = Color32::from_rgb(34, 170, 90);
const AMBER: Color32 = Color32::from_rgb(220, 140, 20);
const RED: Color32 = Color32::from_rgb(220, 60, 60);
const GREY: Color32 = Color32::from_rgb(140, 140, 140);
const ACCENT: Color32 = Color32::from_rgb(20, 140, 130);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Machines,
    Networks,
    Tunnels,
    Ssh,
    Api,
    Computer,
    Terminals,
}

enum Modal {
    Codes { network: String, public: String, private: String, show_private: bool },
    Token { name: String, token: String, url: String },
    Confirm { text: String, op: IpcOp, done: String },
}

struct Toast {
    text: String,
    error: bool,
    at: Instant,
}

#[derive(Default)]
struct Forms {
    filter: String,
    net_name: String,
    add_code: String,
    user: String,
    term: String,
    tun_name: String,
    tun_node: String,
    tun_target: String,
    tun_listen: String,
    rule_hosts: String,
    rule_users: String,
    rule_password: String,
    rule_key: String,
    token_name: String,
    token_scope: String,
    update_from: String,
    node_code: String,
    show_passwords: bool,
}

/// This computer as a node: the node may run inside this window, or be installed as a service.
#[derive(Default)]
struct LocalNode {
    stop: Option<Arc<AtomicBool>>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Memberships read from `gangli.node`: (network, admitted, name on the network, last error).
    networks: Vec<(String, bool, Option<String>, Option<String>)>,
    installed: bool,
    /// The binary of the installed service, wherever it is.
    installed_at: Option<PathBuf>,
    /// gangli starts at login, in the tray.
    autostart: bool,
    read_at: Option<Instant>,
    /// Output of `gangli install` / `uninstall` run from here (None while it runs).
    job: Option<Arc<Mutex<Option<String>>>>,
    job_output: String,
}

pub struct App {
    client: Client,
    page: Page,
    status: Value,
    nodes: Vec<NodeView>,
    tunnels: Vec<Value>,
    ssh: Value,
    api: Value,
    terms: HashMap<String, Vec<Value>>,
    selected: Option<String>,
    polled: Option<Instant>,
    terms_polled: Option<Instant>,
    inflight: HashSet<Tag>,
    toasts: Vec<Toast>,
    modal: Option<Modal>,
    f: Forms,
    node: LocalNode,
    quit: bool,
    show: bool,
    styled: bool,
    /// Text to put on the clipboard at the next frame.
    copy: Option<String>,
    /// Integrated terminals (only when the console lives in this window).
    tabs: Vec<super::term::TermTab>,
    tab_active: usize,
}

fn target(n: &NodeView) -> String {
    format!("{}.{}", n.name, n.network)
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

fn dot(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.5, color);
}

fn exe_dir() -> PathBuf {
    crate::paths::exe_dir()
}

/// Show a file in the system's file manager (the user's own tools).
fn reveal(path: &std::path::Path) {
    #[cfg(windows)]
    let r = std::process::Command::new("explorer.exe").arg(format!("/select,{}", path.display())).spawn();
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg("-R").arg(path).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let r = std::process::Command::new("xdg-open").arg(path.parent().unwrap_or(path)).spawn();
    let _ = r;
}

/// Start a launcher command line (PuTTY, ssh in a terminal, WinSCP…).
fn spawn_argv(argv: &[String]) -> Result<(), String> {
    let Some((prog, args)) = argv.split_first() else { return Err("empty launcher command".into()) };
    let mut c = std::process::Command::new(prog);
    c.args(args).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    c.spawn().map(|_| ()).map_err(|e| format!("{prog}: {e}"))
}

/// Run gangli (this binary unless `exe`) with `args` (install / uninstall: they ask for elevation
/// themselves) and keep its output.
fn run_gangli(exe: Option<PathBuf>, args: Vec<String>, out: Arc<Mutex<Option<String>>>) {
    std::thread::spawn(move || {
        let r = exe.map(Ok).unwrap_or_else(std::env::current_exe).and_then(|exe| {
            let mut c = std::process::Command::new(exe);
            c.args(&args).stdin(std::process::Stdio::null());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
            }
            c.output()
        });
        let text = match r {
            Ok(o) => {
                let mut t = String::from_utf8_lossy(&o.stdout).to_string();
                t.push_str(&String::from_utf8_lossy(&o.stderr));
                if !o.status.success() && t.trim().is_empty() {
                    t = format!("failed ({})", o.status);
                }
                t
            }
            Err(e) => e.to_string(),
        };
        *out.lock().unwrap() = Some(text);
    });
}

impl App {
    pub fn new(client: Client) -> App {
        let mut f = Forms { token_scope: "read".into(), ..Default::default() };
        f.node_code = crate::embed::embedded_code().unwrap_or_default();
        let mut app = App {
            client,
            page: Page::Machines,
            status: Value::Null,
            nodes: vec![],
            tunnels: vec![],
            ssh: Value::Null,
            api: Value::Null,
            terms: HashMap::new(),
            selected: None,
            polled: None,
            terms_polled: None,
            inflight: HashSet::new(),
            toasts: vec![],
            modal: None,
            f,
            node: LocalNode::default(),
            quit: false,
            show: false,
            styled: false,
            copy: None,
            tabs: vec![],
            tab_active: 0,
        };
        app.read_node();
        // A binary made for a relative (with an embedded code), or a computer with no network yet: start on
        // "This computer".
        let consoles_networks = crate::console::load_state().map(|s| s.networks.len()).unwrap_or(0);
        if consoles_networks == 0 && (crate::embed::embedded_code().is_some() || !app.node.networks.is_empty() || app.node.installed) {
            app.page = Page::Computer;
        }
        // For screenshots in tests.
        app.page = match std::env::var("GANGLI_GUI_PAGE").unwrap_or_default().as_str() {
            "machines" => Page::Machines,
            "networks" => Page::Networks,
            "tunnels" => Page::Tunnels,
            "ssh" => Page::Ssh,
            "api" => Page::Api,
            "computer" => Page::Computer,
            _ => app.page,
        };
        // For tests: open an integrated terminal on this machine at once.
        if let Ok(t) = std::env::var("GANGLI_GUI_TERM") {
            let name = t.split('.').next().unwrap_or(&t).to_string();
            app.open_term(&t, &name, None, None);
        }
        app.call(Tag::Ssh, IpcOp::SshStatus);
        app
    }

    pub fn set_waker(&self, w: super::Waker) {
        self.client.set_waker(w);
    }

    /// True once the window or the tray asked to quit.
    pub fn quit_requested(&self) -> bool {
        self.quit
    }

    /// True once (another gangli was started here): the window should come up.
    pub fn take_show(&mut self) -> bool {
        std::mem::take(&mut self.show)
    }

    pub fn tray(&mut self, a: TrayAction) {
        match a {
            TrayAction::Show => self.show = true,
            TrayAction::Quit => self.quit = true,
            TrayAction::Open(name) => {
                if let Some(n) = self.nodes.iter().find(|n| n.name == name) {
                    let t = target(n);
                    self.call(Tag::Launch, IpcOp::Launch { node: t, launcher: None, user: None, term: None });
                }
            }
        }
    }

    pub fn tray_model(&self) -> TrayModel {
        let online: Vec<String> = self.nodes.iter().filter(|n| n.online).map(|n| n.name.clone()).collect();
        TrayModel {
            tooltip: trf(
                format!("gangli: {} of {} machines online", online.len(), self.nodes.len()),
                format!("gangli: macchine online {} su {}", online.len(), self.nodes.len()),
            ),
            online,
            open_label: tr("Open a terminal", "Apri un terminale").into(),
            show_label: tr("Open gangli", "Apri gangli").into(),
            quit_label: tr("Quit", "Esci").into(),
            machines_label: tr("Machines", "Macchine").into(),
        }
    }

    /// Close the console of this window and stop the node it runs.
    pub fn shutdown(&mut self) {
        if let Some(stop) = self.node.stop.take() {
            stop.store(true, Ordering::SeqCst);
        }
        self.client.shutdown();
    }

    fn call(&mut self, tag: Tag, op: IpcOp) {
        // Refreshes are not stacked: one at a time per kind.
        let refresh = matches!(tag, Tag::Status | Tag::Nodes | Tag::Tunnels | Tag::Ssh | Tag::Api | Tag::Terms(_));
        if refresh && !self.inflight.insert(tag.clone()) {
            return;
        }
        self.client.call(tag, op);
    }

    fn toast(&mut self, text: impl Into<String>, error: bool) {
        self.toasts.push(Toast { text: text.into(), error, at: Instant::now() });
    }

    fn read_node(&mut self) {
        self.node.installed_at = crate::service::installed_at();
        self.node.installed = crate::paths::service_installed() || self.node.installed_at.is_some();
        self.node.autostart = super::autostart::enabled();
        self.node.networks.clear();
        if let Ok(st) = crate::state::load() {
            for m in st.networks {
                self.node.networks.push((m.name, m.cert.is_some(), m.node_name, m.last_error));
            }
        }
        self.node.read_at = Some(Instant::now());
    }

    /// Answers and periodic refreshes; also runs while the window is hidden.
    pub fn logic(&mut self, visible: bool) {
        while let Some(r) = self.client.try_recv() {
            self.inflight.remove(&r.tag);
            self.on_reply(r.tag, r.result);
        }
        let every = if visible { REFRESH } else { REFRESH_HIDDEN };
        if self.polled.is_none_or(|t| t.elapsed() >= every) {
            self.polled = Some(Instant::now());
            self.call(Tag::Nodes, IpcOp::Nodes { network: None });
            if visible {
                self.call(Tag::Status, IpcOp::Status);
                match self.page {
                    Page::Tunnels | Page::Machines => self.call(Tag::Tunnels, IpcOp::TunnelList),
                    Page::Ssh => self.call(Tag::Ssh, IpcOp::SshStatus),
                    Page::Api => self.call(Tag::Api, IpcOp::ApiStatus),
                    _ => {}
                }
            }
        }
        if visible && self.node.read_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(3)) {
            self.read_node();
        }
        let finished = self.node.job.as_ref().and_then(|j| j.lock().unwrap().take());
        if let Some(out) = finished {
            self.node.job_output = out;
            self.node.job = None;
            self.read_node();
        }
        if let Some(t) = &self.node.thread {
            if t.is_finished() {
                self.node.thread = None;
                self.node.stop = None;
            }
        }
    }

    fn on_reply(&mut self, tag: Tag, r: Result<Value, String>) {
        let v = match r {
            Ok(v) => v,
            Err(e) => {
                // Periodic refreshes fail quietly (the next one will say it again); actions say why.
                if !matches!(tag, Tag::Nodes | Tag::Status | Tag::Tunnels | Tag::Ssh | Tag::Api | Tag::Terms(_)) {
                    self.toast(e, true);
                }
                return;
            }
        };
        match tag {
            Tag::Status => self.status = v,
            Tag::Nodes => {
                if let Ok(n) = serde_json::from_value::<Vec<NodeView>>(v) {
                    self.nodes = n;
                }
            }
            Tag::Tunnels => self.tunnels = v.as_array().cloned().unwrap_or_default(),
            Tag::Ssh => self.ssh = v,
            Tag::Api => self.api = v,
            Tag::Terms(id) => {
                self.terms.insert(id, v.as_array().cloned().unwrap_or_default());
            }
            Tag::Codes => {
                self.modal = Some(Modal::Codes {
                    network: s(&v["network"]).into(),
                    public: s(&v["public_code"]).into(),
                    private: s(&v["private_code"]).into(),
                    show_private: false,
                });
                self.call(Tag::Status, IpcOp::Status);
            }
            Tag::Token => {
                self.modal = Some(Modal::Token { name: s(&v["name"]).into(), token: s(&v["token"]).into(), url: s(&v["url"]).into() });
                self.call(Tag::Api, IpcOp::ApiStatus);
            }
            Tag::Launch => {
                let argv: Vec<String> = serde_json::from_value(v["argv"].clone()).unwrap_or_default();
                match spawn_argv(&argv) {
                    Ok(()) => {
                        let msg = trf(format!("{} started", s(&v["launcher"])), format!("{} avviato", s(&v["launcher"])));
                        self.toast(msg, false)
                    }
                    Err(e) => self.toast(
                        trf(
                            format!("cannot start {e} (fix its path in the [[launcher]] list of gangli.console)"),
                            format!("non riesco ad avviare {e} (correggi il percorso nei [[launcher]] di gangli.console)"),
                        ),
                        true,
                    ),
                }
            }
            Tag::CopyPassword => {
                self.copy = Some(s(&v["password"]).to_string());
                self.toast(tr("Password copied", "Password copiata"), false);
            }
            Tag::CopySsh => {
                self.copy = Some(format!("ssh -p {} {}@127.0.0.1", v["port"].as_u64().unwrap_or(2222), s(&v["login"])));
                self.toast(tr("ssh command copied", "Comando ssh copiato"), false);
            }
            Tag::Done(msg) => {
                if let Some(list) = v.as_array() {
                    // Update: one result per machine.
                    for r in list {
                        let name = s(&r["node"]).to_string();
                        if r["ok"].as_bool() == Some(true) {
                            self.toast(format!("{name}: {}", tr("updating", "aggiornamento in corso")), false);
                        } else {
                            self.toast(format!("{name}: {}", s(&r["error"])), true);
                        }
                    }
                } else if !msg.is_empty() {
                    self.toast(msg, false);
                }
                self.polled = None;
                self.call(Tag::Tunnels, IpcOp::TunnelList);
                self.call(Tag::Ssh, IpcOp::SshStatus);
                self.call(Tag::Api, IpcOp::ApiStatus);
            }
            Tag::Show => self.show = true,
        }
    }

    fn setup_style(&mut self, ctx: &egui::Context) {
        if self.styled {
            return;
        }
        self.styled = true;
        ctx.all_styles_mut(|st| {
            use egui::{FontFamily, FontId, TextStyle};
            st.text_styles.insert(TextStyle::Body, FontId::new(14.5, FontFamily::Proportional));
            st.text_styles.insert(TextStyle::Button, FontId::new(14.5, FontFamily::Proportional));
            st.text_styles.insert(TextStyle::Heading, FontId::new(21.0, FontFamily::Proportional));
            st.text_styles.insert(TextStyle::Small, FontId::new(12.0, FontFamily::Proportional));
            st.text_styles.insert(TextStyle::Monospace, FontId::new(13.5, FontFamily::Monospace));
            st.spacing.item_spacing = egui::vec2(8.0, 7.0);
            st.spacing.button_padding = egui::vec2(10.0, 4.0);
            st.spacing.interact_size.y = 26.0;
            st.visuals.selection.bg_fill = ACCENT;
            st.visuals.selection.stroke.color = Color32::WHITE;
            if !st.visuals.dark_mode {
                st.visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, Color32::from_gray(205));
                st.visuals.extreme_bg_color = Color32::WHITE;
            }
            st.visuals.hyperlink_color = ACCENT;
        });
    }

    /// One frame of the window.
    pub fn ui(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        self.setup_style(&ctx);
        if let Some(text) = self.copy.take() {
            ctx.copy_text(text);
        }
        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui));
        if let Some(e) = self.client.broken().map(str::to_string) {
            egui::CentralPanel::default_margins().show(ui, |ui| {
                ui.heading(tr("The console could not start", "La console non è partita"));
                ui.label(RichText::new(e).color(RED));
            });
            return;
        }
        if self.page == Page::Machines && self.selected.is_some() {
            egui::Panel::right("details").resizable(true).default_size(420.0).min_size(320.0).show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| self.details(ui));
            });
        }
        if self.page == Page::Terminals {
            egui::CentralPanel::default_margins().show(ui, |ui| self.terminals(ui));
            self.draw_modal(&ctx);
            self.draw_toasts(&ctx);
            return;
        }
        egui::CentralPanel::default_margins().show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match self.page {
                Page::Machines => self.machines(ui),
                Page::Networks => self.networks(ui),
                Page::Tunnels => self.tunnels_page(ui),
                Page::Ssh => self.ssh_page(ui),
                Page::Api => self.api_page(ui),
                Page::Computer => self.computer(ui),
                Page::Terminals => {}
            });
        });
        self.draw_modal(&ctx);
        self.draw_toasts(&ctx);
        // Answers arrive through the waker; this keeps "x ago" times moving.
        ctx.request_repaint_after(Duration::from_secs(1));
    }

    fn top_bar(&mut self, ui: &mut Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("gangli").strong().size(19.0).color(ACCENT));
            ui.add_space(10.0);
            let pages = [
                (Page::Machines, tr("Machines", "Macchine")),
                (Page::Networks, tr("Networks", "Reti")),
                (Page::Tunnels, tr("Tunnels", "Tunnel")),
                (Page::Ssh, "SSH"),
                (Page::Api, "API"),
                (Page::Computer, tr("This computer", "Questo computer")),
            ];
            for (p, label) in pages {
                if ui.selectable_label(self.page == p, label).clicked() {
                    self.page = p;
                    self.polled = None;
                }
            }
            if !self.tabs.is_empty() {
                let label = trf(format!("Terminals ({})", self.tabs.len()), format!("Terminali ({})", self.tabs.len()));
                if ui.selectable_label(self.page == Page::Terminals, label).clicked() {
                    self.page = Page::Terminals;
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button(tr("Quit", "Esci")).on_hover_text(tr("Close gangli and its console", "Chiude gangli e la sua console")).clicked() {
                    self.quit = true;
                }
                let lang = if italian() { "EN" } else { "IT" };
                if ui.small_button(lang).on_hover_text(tr("Italiano", "English")).clicked() {
                    set_italian(!italian());
                }
                let online = self.nodes.iter().filter(|n| n.online).count();
                let txt = trf(format!("{online} of {} online", self.nodes.len()), format!("{online} online su {}", self.nodes.len()));
                ui.label(RichText::new(txt).weak());
                if !self.client.owns_console() && self.client.broken().is_none() {
                    ui.label(RichText::new(tr("(console of `gangli console run`)", "(console di `gangli console run`)")).weak());
                }
            });
        });
        ui.add_space(2.0);
    }

    // ----- Machines -----

    fn machines(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.heading(tr("Machines", "Macchine"));
            ui.add_space(12.0);
            ui.add(egui::TextEdit::singleline(&mut self.f.filter).hint_text(tr("filter", "filtra")).desired_width(180.0));
        });
        ui.add_space(4.0);
        let nets = self.status["networks"].as_array().map(|a| a.len()).unwrap_or(0);
        if self.nodes.is_empty() {
            ui.add_space(20.0);
            if nets == 0 {
                ui.label(tr(
                    "No network yet. Create one in Networks, or add the one you already have with its private code.",
                    "Ancora nessuna rete. Creane una in Reti, o aggiungi quella che hai già con il suo codice privato.",
                ));
                if ui.button(tr("Go to Networks", "Vai a Reti")).clicked() {
                    self.page = Page::Networks;
                }
            } else {
                ui.label(tr(
                    "No machine yet. Run gangli on a machine with the network's public code (Networks → Codes), or send it a prepared gangli.",
                    "Ancora nessuna macchina. Avvia gangli su una macchina con il codice pubblico della rete (Reti → Codici), o mandale un gangli preparato.",
                ));
            }
            return;
        }
        let filter = self.f.filter.to_lowercase();
        let many_nets = nets > 1;
        let t = now();
        let mut nodes = self.nodes.clone();
        nodes.sort_by(|a, b| b.online.cmp(&a.online).then(a.name.cmp(&b.name)));
        egui::Grid::new("machines").striped(true).num_columns(if many_nets { 7 } else { 6 }).spacing([14.0, 8.0]).show(ui, |ui| {
            ui.label("");
            ui.label(RichText::new(tr("Name", "Nome")).strong());
            ui.label(RichText::new(tr("System", "Sistema")).strong());
            ui.label(RichText::new(tr("LAN address", "Indirizzo LAN")).strong());
            ui.label(RichText::new(tr("Link", "Collegamento")).strong());
            ui.label(RichText::new(tr("Since", "Da")).strong());
            if many_nets {
                ui.label(RichText::new(tr("Network", "Rete")).strong());
            }
            ui.end_row();
            for n in &nodes {
                let inv = n.inventory.clone().unwrap_or_default();
                let hay = format!("{} {} {} {}", n.name, n.network, inv.os_label(), inv.ips.join(" ")).to_lowercase();
                if !filter.is_empty() && !hay.contains(&filter) {
                    continue;
                }
                dot(ui, if n.online { GREEN } else { GREY });
                let sel = self.selected.as_deref() == Some(n.id.as_str());
                if ui.selectable_label(sel, RichText::new(&n.name).strong()).clicked() {
                    self.selected = if sel { None } else { Some(n.id.clone()) };
                    self.terms_polled = None;
                }
                ui.label(inv.os_label());
                ui.label(inv.ips.first().cloned().unwrap_or_default());
                link_label(ui, n);
                let when = if n.online {
                    n.since.map(|s| human_ago(t.saturating_sub(s))).unwrap_or_default()
                } else if n.last_seen > 0 {
                    trf(format!("offline, seen {} ago", human_ago(t.saturating_sub(n.last_seen))), format!("offline, visto {} fa", human_ago(t.saturating_sub(n.last_seen))))
                } else {
                    "offline".into()
                };
                ui.label(when);
                if many_nets {
                    ui.label(&n.network);
                }
                ui.end_row();
            }
        });
    }

    fn details(&mut self, ui: &mut Ui) {
        let Some(id) = self.selected.clone() else { return };
        let Some(n) = self.nodes.iter().find(|n| n.id == id).cloned() else {
            self.selected = None;
            return;
        };
        let inv = n.inventory.clone().unwrap_or_default();
        let tgt = target(&n);
        ui.horizontal(|ui| {
            dot(ui, if n.online { GREEN } else { GREY });
            ui.heading(&n.name);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("×").clicked() {
                    self.selected = None;
                }
            });
        });
        ui.label(RichText::new(format!("{} · {}", n.network, inv.os_label())).weak());
        ui.add_space(6.0);

        // Open a terminal / files.
        ui.label(RichText::new(tr("Open", "Apri")).strong());
        ui.horizontal(|ui| {
            ui.label(tr("as", "come"));
            ui.add(egui::TextEdit::singleline(&mut self.f.user).hint_text(tr("default", "predefinito")).desired_width(110.0));
            if !inv.users.is_empty() {
                egui::ComboBox::from_id_salt("users").selected_text(tr("choose", "scegli")).width(80.0).show_ui(ui, |ui| {
                    for u in ["system", "desktop"].iter().map(|s| s.to_string()).chain(inv.users.iter().cloned()) {
                        if ui.selectable_label(self.f.user == u, &u).clicked() {
                            self.f.user = u.clone();
                        }
                    }
                });
            }
        });
        ui.horizontal(|ui| {
            ui.label(tr("persistent terminal", "terminale persistente"));
            ui.add(egui::TextEdit::singleline(&mut self.f.term).hint_text(tr("none", "nessuno")).desired_width(110.0));
        });
        let user = (!self.f.user.trim().is_empty()).then(|| self.f.user.trim().to_string());
        let term = (!self.f.term.trim().is_empty()).then(|| self.f.term.trim().to_string());
        let launchers: Vec<String> = self.ssh["launchers"].as_array().map(|a| a.iter().map(|l| s(&l["name"]).to_string()).collect()).unwrap_or_default();
        ui.horizontal_wrapped(|ui| {
            if self.client.console().is_some()
                && ui
                    .add_enabled(n.online, egui::Button::new(RichText::new(tr("Terminal here", "Terminale qui")).strong()))
                    .on_hover_text(tr("A terminal inside this window", "Un terminale dentro questa finestra"))
                    .clicked()
            {
                self.open_term(&tgt, &n.name, user.clone(), term.clone());
            }
            for l in &launchers {
                if ui.add_enabled(n.online, egui::Button::new(l)).clicked() {
                    self.call(Tag::Launch, IpcOp::Launch { node: tgt.clone(), launcher: Some(l.clone()), user: user.clone(), term: term.clone() });
                }
            }
        });
        ui.horizontal_wrapped(|ui| {
            if ui.button(tr("Copy password", "Copia password")).clicked() {
                self.call(Tag::CopyPassword, IpcOp::SshPassword { node: tgt.clone(), user: user.clone() });
            }
            if ui.button(tr("Copy ssh command", "Copia comando ssh")).clicked() {
                self.call(Tag::CopySsh, IpcOp::SshPassword { node: tgt.clone(), user: user.clone() });
            }
        });
        if !n.online {
            ui.label(RichText::new(tr("Offline: it comes back by itself when the machine is on.", "Offline: torna da sola quando la macchina è accesa.")).weak());
        }

        // Persistent terminals.
        if n.online {
            if self.terms_polled.is_none_or(|t| t.elapsed() >= Duration::from_secs(5)) {
                self.terms_polled = Some(Instant::now());
                self.call(Tag::Terms(n.id.clone()), IpcOp::TermList { node: tgt.clone() });
            }
            let terms = self.terms.get(&n.id).cloned().unwrap_or_default();
            if !terms.is_empty() {
                ui.add_space(6.0);
                ui.label(RichText::new(tr("Persistent terminals", "Terminali persistenti")).strong());
                for t in terms {
                    let name = s(&t["name"]).to_string();
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&name).monospace());
                        ui.label(RichText::new(format!("{} · {}", s(&t["user"]), human_ago(now().saturating_sub(t["started"].as_u64().unwrap_or(0))))).weak());
                        let u = Some(s(&t["user"]).to_string()).filter(|u| !u.is_empty());
                        if self.client.console().is_some() && ui.small_button(tr("here", "qui")).clicked() {
                            self.open_term(&tgt, &n.name, u.clone(), Some(name.clone()));
                        }
                        if ui.small_button(tr("open", "apri")).clicked() {
                            self.call(Tag::Launch, IpcOp::Launch { node: tgt.clone(), launcher: None, user: u.clone(), term: Some(name.clone()) });
                        }
                        if ui.small_button(tr("close", "chiudi")).clicked() {
                            self.call(Tag::Done(trf(format!("terminal {name} closed"), format!("terminale {name} chiuso"))), IpcOp::TermKill { node: tgt.clone(), name: name.clone() });
                            self.terms_polled = None;
                        }
                    });
                }
            }
        }

        // Tunnels of this machine.
        ui.add_space(8.0);
        ui.label(RichText::new(tr("Tunnels", "Tunnel")).strong());
        let mine: Vec<Value> = self.tunnels.iter().filter(|t| s(&t["node"]) == tgt || s(&t["node"]) == n.name).cloned().collect();
        for t in &mine {
            ui.horizontal(|ui| {
                dot(ui, if t["active"].as_bool() == Some(true) { GREEN } else { GREY });
                ui.label(RichText::new(format!("{} → {}", s(&t["listen"]), s(&t["target"]))).monospace());
            });
        }
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.f.tun_target).hint_text(tr("their address, e.g. 192.168.1.20:80", "indirizzo da loro, es. 192.168.1.20:80")).desired_width(200.0));
            ui.add(egui::TextEdit::singleline(&mut self.f.tun_listen).hint_text(tr("local port", "porta locale")).desired_width(80.0));
            if ui.button(tr("Add", "Aggiungi")).clicked() {
                self.add_tunnel(tgt.clone(), n.name.clone());
            }
        });

        // What the machine is.
        ui.add_space(8.0);
        ui.label(RichText::new(tr("Machine", "Macchina")).strong());
        egui::Grid::new("inv").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
            let mut row = |k: &str, v: String| {
                ui.label(RichText::new(k).weak());
                ui.add(egui::Label::new(v).wrap());
                ui.end_row();
            };
            row(tr("System", "Sistema"), format!("{} ({}, {})", inv.os_label(), inv.kernel, inv.arch));
            row("CPU", format!("{} × {}", inv.cpus, inv.cpu));
            row("RAM", trf(format!("{} free of {}", human_bytes(inv.mem_avail), human_bytes(inv.mem_total)), format!("{} liberi su {}", human_bytes(inv.mem_avail), human_bytes(inv.mem_total))));
            for d in &inv.disks {
                row(tr("Disk", "Disco"), format!("{}  {} / {}", d.mount, human_bytes(d.avail), human_bytes(d.total)));
            }
            row(tr("LAN addresses", "Indirizzi LAN"), inv.ips.join(", "));
            row(tr("Users", "Utenti"), inv.users.join(", "));
            let access = if inv.full_access { tr("full", "completo") } else { tr("this user only", "solo questo utente") };
            row(tr("Runs as", "Gira come"), format!("{} ({access})", inv.service_user));
            row(tr("Up for", "Acceso da"), human_ago(inv.uptime));
            row("gangli", inv.version.clone());
            if let Some(p) = &n.path {
                row(tr("Peer address", "Indirizzo"), p.remote.clone());
            }
        });

        // Update, forget.
        ui.add_space(8.0);
        ui.label(RichText::new(tr("Update", "Aggiorna")).strong());
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.f.update_from).hint_text(tr("folder with a signed release", "cartella con una release firmata")).desired_width(220.0));
            if ui.add_enabled(n.online && !self.f.update_from.trim().is_empty(), egui::Button::new(tr("Update", "Aggiorna"))).clicked() {
                self.call(Tag::Done(String::new()), IpcOp::Update { nodes: vec![tgt.clone()], from: self.f.update_from.trim().to_string() });
            }
        });
        ui.add_space(8.0);
        if ui.button(RichText::new(tr("Forget this machine", "Dimentica questa macchina")).color(RED)).clicked() {
            self.modal = Some(Modal::Confirm {
                text: trf(
                    format!("Forget {}? It is removed from the network; to come back it must be admitted again.", n.name),
                    format!("Dimenticare {}? Viene tolta dalla rete; per tornare dovrà essere ammessa di nuovo.", n.name),
                ),
                op: IpcOp::Forget { node: tgt.clone() },
                done: trf(format!("{} forgotten", n.name), format!("{} dimenticata", n.name)),
            });
        }
    }

    fn add_tunnel(&mut self, node: String, node_name: String) {
        let target = self.f.tun_target.trim().to_string();
        if !target.contains(':') {
            self.toast(tr("Write the address as host:port", "Scrivi l'indirizzo come host:porta"), true);
            return;
        }
        let listen = self.f.tun_listen.trim().to_string();
        let mut name = self.f.tun_name.trim().to_string();
        if name.is_empty() {
            let port = if listen.is_empty() { target.rsplit(':').next().unwrap_or("").to_string() } else { listen.rsplit(':').next().unwrap_or("").to_string() };
            name = crate::util::slug(&format!("{node_name}-{port}"));
        }
        self.call(
            Tag::Done(trf(format!("tunnel {name} added"), format!("tunnel {name} aggiunto"))),
            IpcOp::TunnelAdd { name, node, target, listen: (!listen.is_empty()).then_some(listen) },
        );
        self.f.tun_target.clear();
        self.f.tun_listen.clear();
        self.f.tun_name.clear();
    }

    // ----- Networks -----

    fn networks(&mut self, ui: &mut Ui) {
        ui.heading(tr("Networks", "Reti"));
        ui.label(RichText::new(tr(
            "A network is a key. Its public code makes a machine a node; its private code makes a console an owner: keep it for yourself.",
            "Una rete è una chiave. Il codice pubblico rende nodo una macchina; il codice privato rende proprietaria una console: tienilo per te.",
        )).weak());
        ui.add_space(8.0);
        let nets = self.status["networks"].as_array().cloned().unwrap_or_default();
        for n in &nets {
            let name = s(&n["name"]).to_string();
            egui::Frame::group(ui.style()).inner_margin(10).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&name).strong().size(16.0));
                    ui.label(RichText::new(trf(
                        format!("{} of {} machines online · {} consoles", n["online"], n["nodes"], n["consoles"]),
                        format!("macchine online: {} su {} · console: {}", n["online"], n["nodes"], n["consoles"]),
                    )).weak());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button(RichText::new(tr("Remove", "Rimuovi")).color(RED)).clicked() {
                            self.modal = Some(Modal::Confirm {
                                text: trf(
                                    format!("Remove {name} from this console? Without its private code you cannot add it back."),
                                    format!("Togliere {name} da questa console? Senza il suo codice privato non potrai riaggiungerla."),
                                ),
                                op: IpcOp::NetworkRemove { name: name.clone() },
                                done: trf(format!("{name} removed"), format!("{name} rimossa")),
                            });
                        }
                        if ui.button(tr("Codes", "Codici")).clicked() {
                            self.call(Tag::Codes, IpcOp::NetworkCodes { name: name.clone() });
                        }
                    });
                });
            });
            ui.add_space(4.0);
        }
        ui.add_space(10.0);
        ui.label(RichText::new(tr("New network", "Nuova rete")).strong());
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.f.net_name).hint_text(tr("name, e.g. family", "nome, es. famiglia")).desired_width(200.0));
            if ui.add_enabled(!self.f.net_name.trim().is_empty(), egui::Button::new(tr("Create", "Crea"))).clicked() {
                self.call(Tag::Codes, IpcOp::NetworkCreate { name: self.f.net_name.trim().to_string() });
                self.f.net_name.clear();
            }
        });
        ui.add_space(6.0);
        ui.label(RichText::new(tr("A network you already own (from another console)", "Una rete che hai già (da un'altra console)")).strong());
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.f.add_code).password(true).hint_text(tr("private code gangli-key-…", "codice privato gangli-key-…")).desired_width(300.0));
            if ui.add_enabled(!self.f.add_code.trim().is_empty(), egui::Button::new(tr("Add", "Aggiungi"))).clicked() {
                self.call(Tag::Done(tr("Network added", "Rete aggiunta").into()), IpcOp::NetworkAdd { code: self.f.add_code.trim().to_string(), name: None });
                self.f.add_code.clear();
            }
        });
    }

    // ----- Tunnels -----

    fn tunnels_page(&mut self, ui: &mut Ui) {
        ui.heading(tr("Tunnels", "Tunnel"));
        ui.label(RichText::new(tr(
            "A local port that reaches an address on a machine's network; saved, and back by itself while gangli is open.",
            "Una porta locale che raggiunge un indirizzo nella rete di una macchina; salvato, e riattivato da solo mentre gangli è aperto.",
        )).weak());
        ui.add_space(8.0);
        let tunnels = self.tunnels.clone();
        if !tunnels.is_empty() {
            egui::Grid::new("tunnels").striped(true).num_columns(6).spacing([14.0, 8.0]).show(ui, |ui| {
                for h in [tr("On", "Attivo"), tr("Name", "Nome"), tr("Machine", "Macchina"), tr("Local", "Locale"), tr("Their address", "Indirizzo da loro"), ""] {
                    ui.label(RichText::new(h).strong());
                }
                ui.end_row();
                for t in &tunnels {
                    let name = s(&t["name"]).to_string();
                    let mut on = t["enabled"].as_bool() == Some(true);
                    ui.horizontal(|ui| {
                        if ui.checkbox(&mut on, "").changed() {
                            self.call(Tag::Done(String::new()), IpcOp::TunnelSet { name: name.clone(), enabled: on });
                        }
                        dot(ui, if t["active"].as_bool() == Some(true) { GREEN } else { GREY });
                    });
                    ui.label(&name);
                    ui.label(s(&t["node"]));
                    ui.label(RichText::new(s(&t["listen"])).monospace());
                    ui.label(RichText::new(s(&t["target"])).monospace());
                    if ui.small_button(tr("remove", "rimuovi")).clicked() {
                        self.call(Tag::Done(trf(format!("tunnel {name} removed"), format!("tunnel {name} rimosso"))), IpcOp::TunnelRemove { name: name.clone() });
                    }
                    ui.end_row();
                }
            });
        }
        ui.add_space(10.0);
        ui.label(RichText::new(tr("New tunnel", "Nuovo tunnel")).strong());
        ui.horizontal_wrapped(|ui| {
            let names: Vec<(String, String)> = self.nodes.iter().map(|n| (target(n), n.name.clone())).collect();
            let shown = names.iter().find(|(t, _)| *t == self.f.tun_node).map(|(_, n)| n.clone()).unwrap_or_else(|| tr("machine", "macchina").into());
            egui::ComboBox::from_id_salt("tun_node").selected_text(shown).show_ui(ui, |ui| {
                for (t, n) in &names {
                    ui.selectable_value(&mut self.f.tun_node, t.clone(), n);
                }
            });
            ui.add(egui::TextEdit::singleline(&mut self.f.tun_target).hint_text(tr("their address host:port", "indirizzo da loro host:porta")).desired_width(200.0));
            ui.add(egui::TextEdit::singleline(&mut self.f.tun_listen).hint_text(tr("local port", "porta locale")).desired_width(90.0));
            ui.add(egui::TextEdit::singleline(&mut self.f.tun_name).hint_text(tr("name (optional)", "nome (facoltativo)")).desired_width(130.0));
            if ui.add_enabled(!self.f.tun_node.is_empty(), egui::Button::new(tr("Add", "Aggiungi"))).clicked() {
                let node = self.f.tun_node.clone();
                let name = names.iter().find(|(t, _)| *t == node).map(|(_, n)| n.clone()).unwrap_or_default();
                self.add_tunnel(node, name);
            }
        });
    }

    // ----- SSH -----

    fn ssh_page(&mut self, ui: &mut Ui) {
        ui.heading(tr("SSH gateway", "Gateway SSH"));
        let listen = s(&self.ssh["listen"]).to_string();
        ui.label(trf(
            format!("Any SSH client reaches the machines at {listen} with the login user+machine (e.g. root+nas)."),
            format!("Qualsiasi client SSH raggiunge le macchine su {listen} con il login utente+macchina (es. root+nas)."),
        ));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(tr("Login rules", "Regole di accesso")).strong());
            ui.checkbox(&mut self.f.show_passwords, tr("show passwords", "mostra le password"));
        });
        ui.label(RichText::new(tr("The most specific rule wins; * means any.", "Vince la regola più specifica; * vuol dire qualsiasi.")).weak());
        let rules = self.ssh["logins"].as_array().cloned().unwrap_or_default();
        egui::Grid::new("rules").striped(true).num_columns(6).spacing([14.0, 8.0]).show(ui, |ui| {
            for h in ["#", tr("Machines", "Macchine"), tr("Users", "Utenti"), "Password", tr("Keys", "Chiavi"), ""] {
                ui.label(RichText::new(h).strong());
            }
            ui.end_row();
            for r in &rules {
                let index = r["index"].as_u64().unwrap_or(0) as usize;
                let list = |v: &Value| v.as_array().map(|a| a.iter().map(|x| s(x).to_string()).collect::<Vec<_>>().join(", ")).unwrap_or_default();
                ui.label(index.to_string());
                ui.label(list(&r["hosts"]));
                ui.label(list(&r["users"]));
                let pw = s(&r["password"]).to_string();
                ui.horizontal(|ui| {
                    ui.label(RichText::new(if self.f.show_passwords { pw.clone() } else { "••••••••".into() }).monospace());
                    if ui.small_button(tr("copy", "copia")).clicked() {
                        self.copy = Some(pw.clone());
                        self.toast(tr("Password copied", "Password copiata"), false);
                    }
                });
                ui.label(r["keys"].to_string());
                if ui.small_button(tr("remove", "rimuovi")).clicked() {
                    self.call(Tag::Done(tr("Rule removed", "Regola rimossa").into()), IpcOp::SshLoginRemove { index });
                }
                ui.end_row();
            }
        });
        ui.add_space(8.0);
        ui.label(RichText::new(tr("New rule", "Nuova regola")).strong());
        ui.horizontal_wrapped(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.f.rule_hosts).hint_text(tr("machines (comma), * = all", "macchine (virgola), * = tutte")).desired_width(180.0));
            ui.add(egui::TextEdit::singleline(&mut self.f.rule_users).hint_text(tr("users (comma), * = all", "utenti (virgola), * = tutti")).desired_width(160.0));
            ui.add(egui::TextEdit::singleline(&mut self.f.rule_password).hint_text(tr("password (empty: generated)", "password (vuota: generata)")).desired_width(170.0));
        });
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.f.rule_key).hint_text(tr("OpenSSH public key (optional)", "chiave pubblica OpenSSH (facoltativa)")).desired_width(420.0));
            if ui.button(tr("Add", "Aggiungi")).clicked() {
                let split = |s: &str| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect::<Vec<_>>();
                let op = IpcOp::SshLoginAdd {
                    hosts: split(&self.f.rule_hosts),
                    users: split(&self.f.rule_users),
                    password: (!self.f.rule_password.trim().is_empty()).then(|| self.f.rule_password.trim().to_string()),
                    key: (!self.f.rule_key.trim().is_empty()).then(|| self.f.rule_key.trim().to_string()),
                };
                self.call(Tag::Done(tr("Rule added", "Regola aggiunta").into()), op);
                self.f.rule_hosts.clear();
                self.f.rule_users.clear();
                self.f.rule_password.clear();
                self.f.rule_key.clear();
            }
        });
        ui.add_space(10.0);
        ui.label(RichText::new(tr("Launchers", "Programmi")).strong());
        ui.label(RichText::new(tr("Edit them in gangli.console ([[launcher]]): {login} {password} {port} {node} {user} are filled in.", "Si modificano in gangli.console ([[launcher]]): {login} {password} {port} {node} {user} vengono compilati.")).weak());
        for l in self.ssh["launchers"].as_array().cloned().unwrap_or_default() {
            ui.horizontal(|ui| {
                ui.label(RichText::new(s(&l["name"])).strong());
                ui.label(RichText::new(s(&l["command"])).monospace().weak());
            });
        }
    }

    // ----- API -----

    fn api_page(&mut self, ui: &mut Ui) {
        ui.heading(tr("API for programs and AI agents", "API per programmi e agenti AI"));
        ui.label(RichText::new(tr(
            "A local HTTP API (127.0.0.1 only) while gangli is open. Each program gets its own token, which you can revoke.",
            "Un'API HTTP locale (solo 127.0.0.1) mentre gangli è aperto. Ogni programma ha il suo token, che puoi revocare.",
        )).weak());
        ui.add_space(8.0);
        let mut on = self.api["enabled"].as_bool() == Some(true);
        ui.horizontal(|ui| {
            if ui.checkbox(&mut on, tr("Enabled", "Attiva")).changed() {
                self.call(Tag::Done(String::new()), if on { IpcOp::ApiEnable } else { IpcOp::ApiDisable });
            }
            ui.label(RichText::new(s(&self.api["url"])).monospace());
        });
        let tokens = self.api["tokens"].as_array().cloned().unwrap_or_default();
        if !tokens.is_empty() {
            ui.add_space(6.0);
            egui::Grid::new("tokens").striped(true).num_columns(4).spacing([14.0, 8.0]).show(ui, |ui| {
                for h in [tr("Name", "Nome"), tr("Scope", "Permessi"), tr("Created", "Creato"), ""] {
                    ui.label(RichText::new(h).strong());
                }
                ui.end_row();
                for t in &tokens {
                    let name = s(&t["name"]).to_string();
                    ui.label(&name);
                    ui.label(s(&t["scope"]));
                    ui.label(trf(format!("{} ago", human_ago(now().saturating_sub(t["created"].as_u64().unwrap_or(0)))), format!("{} fa", human_ago(now().saturating_sub(t["created"].as_u64().unwrap_or(0))))));
                    if ui.small_button(tr("revoke", "revoca")).clicked() {
                        self.call(Tag::Done(trf(format!("token {name} revoked"), format!("token {name} revocato"))), IpcOp::ApiTokenRemove { name: name.clone() });
                    }
                    ui.end_row();
                }
            });
        }
        ui.add_space(10.0);
        ui.label(RichText::new(tr("New token", "Nuovo token")).strong());
        ui.label(RichText::new(tr(
            "read: machines and their details. exec: also commands, jobs, terminals. admin: everything (networks, passwords).",
            "read: macchine e dettagli. exec: anche comandi, job, terminali. admin: tutto (reti, password).",
        )).weak());
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.f.token_name).hint_text(tr("who uses it, e.g. claude", "chi lo usa, es. claude")).desired_width(180.0));
            egui::ComboBox::from_id_salt("scope").selected_text(self.f.token_scope.clone()).show_ui(ui, |ui| {
                for sc in ["read", "exec", "admin"] {
                    ui.selectable_value(&mut self.f.token_scope, sc.to_string(), sc);
                }
            });
            if ui.add_enabled(!self.f.token_name.trim().is_empty(), egui::Button::new(tr("Create", "Crea"))).clicked() {
                self.call(Tag::Token, IpcOp::ApiTokenAdd { name: self.f.token_name.trim().to_string(), scope: self.f.token_scope.clone() });
                self.f.token_name.clear();
            }
        });
    }

    // ----- This computer -----

    fn computer(&mut self, ui: &mut Ui) {
        ui.heading(tr("This computer", "Questo computer"));
        ui.label(RichText::new(crate::version_line()).weak());
        ui.add_space(8.0);
        ui.label(RichText::new(tr("Managed from a console (node)", "Gestito da una console (nodo)")).strong());
        let busy = self.node.job.is_some();
        if self.node.installed {
            ui.label(tr(
                "gangli is installed as a service on this computer: it starts with the computer and its consoles have full access.",
                "gangli è installato come servizio su questo computer: parte con il computer e le sue console hanno accesso completo.",
            ));
            let bin = self.node.installed_at.clone();
            if let Some(b) = &bin {
                ui.label(RichText::new(b.display().to_string()).monospace().weak());
            }
            if ui.add_enabled(!busy, egui::Button::new(tr("Uninstall the service…", "Disinstalla il servizio…"))).clicked() {
                self.start_job(bin, vec!["uninstall".into()]);
            }
        } else {
            for (net, admitted, name, err) in &self.node.networks {
                ui.horizontal(|ui| {
                    dot(ui, if *admitted { GREEN } else { AMBER });
                    let state = if *admitted {
                        trf(format!("admitted as {}", name.clone().unwrap_or_default()), format!("ammesso come {}", name.clone().unwrap_or_default()))
                    } else {
                        tr("waiting for a console of the network", "in attesa di una console della rete").to_string()
                    };
                    ui.label(RichText::new(net).strong());
                    ui.label(state);
                    if let (false, Some(e)) = (*admitted, err) {
                        ui.label(RichText::new(e).color(RED));
                    }
                });
            }
            let running = self.node.stop.is_some();
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.f.node_code).hint_text(tr("public code gangli-pub-…", "codice pubblico gangli-pub-…")).desired_width(320.0));
                if ui.add_enabled(!self.f.node_code.trim().is_empty(), egui::Button::new(tr("Join", "Entra"))).clicked() {
                    match crate::state::add_join(self.f.node_code.trim()) {
                        Ok(name) => self.toast(trf(format!("joined {name}"), format!("entrato in {name}")), false),
                        Err(e) => self.toast(format!("{e:#}"), true),
                    }
                    self.read_node();
                }
            });
            ui.horizontal_wrapped(|ui| {
                if running {
                    dot(ui, GREEN);
                    ui.label(RichText::new(tr("The node runs in this window", "Il nodo è attivo in questa finestra")).color(GREEN));
                    if ui.button(tr("Stop", "Ferma")).clicked() {
                        if let Some(s) = self.node.stop.take() {
                            s.store(true, Ordering::SeqCst);
                        }
                    }
                } else if ui
                    .add_enabled(!self.node.networks.is_empty(), egui::Button::new(tr("Run the node now", "Avvia il nodo adesso")))
                    .on_hover_text(tr("Until gangli is closed, as this user", "Finché gangli è aperto, come questo utente"))
                    .clicked()
                {
                    let stop = Arc::new(AtomicBool::new(false));
                    let s2 = stop.clone();
                    self.node.thread = Some(std::thread::spawn(move || crate::node::run_blocking(s2)));
                    self.node.stop = Some(stop);
                }
                let code = self.f.node_code.trim().to_string();
                if ui
                    .add_enabled(!busy && (!code.is_empty() || !self.node.networks.is_empty()), egui::Button::new(tr("Install as a service…", "Installa come servizio…")))
                    .on_hover_text(tr("Starts with the computer, full access; asks for the administrator once", "Parte con il computer, accesso completo; chiede l'amministratore una volta"))
                    .clicked()
                {
                    if let Some(s) = self.node.stop.take() {
                        s.store(true, Ordering::SeqCst);
                    }
                    let mut args = vec!["install".to_string()];
                    if !code.is_empty() {
                        args.push("--code".into());
                        args.push(code);
                    }
                    self.start_job(None, args);
                }
            });
        }
        if busy {
            ui.label(RichText::new(tr("Working… (confirm the system's request)", "In corso… (conferma la richiesta del sistema)")).weak());
        }
        if !self.node.job_output.is_empty() {
            ui.add(egui::Label::new(RichText::new(self.node.job_output.trim()).monospace()).wrap());
        }
        ui.add_space(14.0);
        ui.label(RichText::new(tr("This window", "Questa finestra")).strong());
        let mut on = self.node.autostart;
        if ui
            .checkbox(&mut on, tr("Start gangli at login, in the tray", "Avvia gangli all'accesso, nella tray"))
            .on_hover_text(tr("Your machines stay listed and the tunnels on without opening gangli", "Le macchine restano in elenco e i tunnel attivi senza aprire gangli"))
            .changed()
        {
            match super::autostart::set(on) {
                Ok(()) => self.node.autostart = on,
                Err(e) => self.toast(format!("{e:#}"), true),
            }
        }
        ui.add_space(14.0);
        ui.label(RichText::new(tr("Files", "File")).strong());
        ui.label(RichText::new(tr("Everything gangli keeps is in these files next to it:", "Tutto quello che gangli conserva è in questi file accanto a lui:")).weak());
        for p in [crate::paths::node_file(), crate::paths::console_file(), crate::paths::log_file()] {
            ui.label(RichText::new(p.display().to_string()).monospace());
        }
        if ui.button(tr("Show the folder", "Mostra la cartella")).clicked() {
            reveal(&crate::paths::console_file());
        }
        let _ = exe_dir;
    }

    /// Run gangli (this binary, or the installed one) with `args`, keeping its output for the page.
    fn start_job(&mut self, exe: Option<PathBuf>, args: Vec<String>) {
        let out = Arc::new(Mutex::new(None));
        self.node.job = Some(out.clone());
        self.node.job_output.clear();
        run_gangli(exe, args, out);
    }

    // ----- Integrated terminals -----

    fn open_term(&mut self, target: &str, name: &str, user: Option<String>, persistent: Option<String>) {
        let Some((rt, c)) = self.client.console() else { return };
        let label = match (&user, &persistent) {
            (Some(u), Some(t)) => format!("{u}@{name}/{t}"),
            (None, Some(t)) => format!("{name}/{t}"),
            (Some(u), None) => format!("{u}@{name}"),
            (None, None) => name.to_string(),
        };
        let tab = super::term::TermTab::open(&rt, c, target.to_string(), label, user, persistent, self.client.waker());
        self.tabs.push(tab);
        self.tab_active = self.tabs.len() - 1;
        self.page = Page::Terminals;
    }

    fn terminals(&mut self, ui: &mut Ui) {
        if self.tabs.is_empty() {
            self.page = Page::Machines;
            return;
        }
        self.tab_active = self.tab_active.min(self.tabs.len() - 1);
        let mut close = None;
        ui.horizontal_wrapped(|ui| {
            for (i, t) in self.tabs.iter().enumerate() {
                let mut title = t.title();
                if title.chars().count() > 40 {
                    title = title.chars().take(39).collect::<String>() + "…";
                }
                if t.ended() {
                    title = format!("({title})");
                }
                if ui.selectable_label(i == self.tab_active, title).clicked() {
                    self.tab_active = i;
                }
                if ui.small_button("×").on_hover_text(tr("Close this terminal", "Chiudi questo terminale")).clicked() {
                    close = Some(i);
                }
                ui.add_space(6.0);
            }
        });
        ui.add_space(2.0);
        if let Some(i) = close {
            self.tabs.remove(i);
            return;
        }
        let active = self.tab_active;
        self.tabs[active].ui(ui);
    }

    // ----- Overlays -----

    fn draw_modal(&mut self, ctx: &egui::Context) {
        let Some(mut m) = self.modal.take() else { return };
        let mut keep = true;
        egui::Modal::new(egui::Id::unique("modal")).show(ctx, |ui| {
            ui.set_max_width(560.0);
            match &mut m {
                Modal::Codes { network, public, private, show_private } => {
                    ui.heading(trf(format!("Network {network}"), format!("Rete {network}")));
                    ui.add_space(6.0);
                    ui.label(RichText::new(tr("Public code: for the machines to manage", "Codice pubblico: per le macchine da gestire")).strong());
                    ui.add(egui::Label::new(RichText::new(public.as_str()).monospace()).wrap());
                    ui.horizontal(|ui| {
                        if ui.button(tr("Copy", "Copia")).clicked() {
                            self.copy = Some(public.clone());
                        }
                        if ui.button(tr("Prepare a gangli with it…", "Prepara un gangli con questo…")).on_hover_text(tr("A copy of this program that joins the network by itself: send it to whoever needs help", "Una copia di questo programma che entra da sola nella rete: mandala a chi va aiutato")).clicked() {
                            let ext = if cfg!(windows) { ".exe" } else { "" };
                            let out = exe_dir().join(format!("gangli-{network}{ext}"));
                            match std::env::current_exe().map_err(anyhow::Error::from).and_then(|me| crate::embed::pack(&me, &out, public)) {
                                Ok(()) => {
                                    self.toast(trf(format!("ready: {}", out.display()), format!("pronto: {}", out.display())), false);
                                    reveal(&out);
                                }
                                Err(e) => self.toast(format!("{e:#}"), true),
                            }
                        }
                    });
                    ui.add_space(10.0);
                    ui.label(RichText::new(tr("Private code: makes a console an owner of the network", "Codice privato: rende una console proprietaria della rete")).strong().color(AMBER));
                    ui.label(RichText::new(tr("Keep it safe and never put it on a managed machine. It cannot be revoked.", "Conservalo bene e non metterlo mai su una macchina gestita. Non si può revocare.")).weak());
                    if *show_private {
                        ui.add(egui::Label::new(RichText::new(private.as_str()).monospace()).wrap());
                    }
                    ui.horizontal(|ui| {
                        ui.checkbox(show_private, tr("show", "mostra"));
                        if ui.button(tr("Copy", "Copia")).clicked() {
                            self.copy = Some(private.clone());
                        }
                    });
                }
                Modal::Token { name, token, url } => {
                    ui.heading(trf(format!("Token {name}"), format!("Token {name}")));
                    ui.label(tr("Shown only now: copy it into the program that uses it.", "Si vede solo adesso: copialo nel programma che lo usa."));
                    ui.add(egui::Label::new(RichText::new(token.as_str()).monospace()).wrap());
                    ui.label(RichText::new(format!("{url}   Authorization: Bearer …")).monospace().weak());
                    if ui.button(tr("Copy", "Copia")).clicked() {
                        self.copy = Some(token.clone());
                    }
                    if self.api["enabled"].as_bool() != Some(true) {
                        ui.label(RichText::new(tr("The API is off: turn it on to use the token.", "L'API è spenta: attivala per usare il token.")).color(AMBER));
                    }
                }
                Modal::Confirm { text, .. } => {
                    ui.label(text.as_str());
                }
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if let Modal::Confirm { op, done, .. } = &m {
                    if ui.button(RichText::new(tr("Yes", "Sì")).color(RED)).clicked() {
                        self.call(Tag::Done(done.clone()), op.clone());
                        keep = false;
                    }
                    if ui.button(tr("Cancel", "Annulla")).clicked() {
                        keep = false;
                    }
                } else if ui.button(tr("Close", "Chiudi")).clicked() {
                    keep = false;
                }
            });
        });
        if keep && !ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.modal = Some(m);
        }
    }

    fn draw_toasts(&mut self, ctx: &egui::Context) {
        self.toasts.retain(|t| t.at.elapsed() < TOAST);
        if self.toasts.is_empty() {
            return;
        }
        egui::Area::new(egui::Id::unique("toasts")).anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-14.0, -14.0)).show(ctx, |ui| {
            for t in &self.toasts {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_max_width(420.0);
                    ui.label(RichText::new(&t.text).color(if t.error { RED } else { ui.visuals().text_color() }));
                });
            }
        });
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

fn link_label(ui: &mut Ui, n: &NodeView) {
    match (&n.path, n.online) {
        (Some(p), true) if p.relay => {
            ui.label(RichText::new(format!("relay {} ms", p.rtt_ms)).color(AMBER))
                .on_hover_text(tr("Through a relay server: slower. Direct links need UDP to get through.", "Passa da un server relay: più lento. Il collegamento diretto ha bisogno che passi l'UDP."));
        }
        (Some(p), true) => {
            ui.label(RichText::new(format!("{} {} ms", tr("direct", "diretto"), p.rtt_ms)).color(GREEN));
        }
        _ => {
            ui.label("");
        }
    }
}
