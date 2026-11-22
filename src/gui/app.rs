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
use super::{TrayAction, TrayModel};
use crate::ipc::{IpcOp, NodeView};
use crate::util::{human_ago, human_bytes, now};

const REFRESH: Duration = Duration::from_secs(2);
const REFRESH_HIDDEN: Duration = Duration::from_secs(8);
const TOAST: Duration = Duration::from_secs(6);

pub(super) const GREEN: Color32 = Color32::from_rgb(34, 170, 90);
const AMBER: Color32 = Color32::from_rgb(220, 140, 20);
const RED: Color32 = Color32::from_rgb(220, 60, 60);
pub(super) const GREY: Color32 = Color32::from_rgb(140, 140, 140);
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
    Token { name: String, token: String, url: String, instructions: String },
    /// Joining from a phone: the QR shown, the answer when it comes (the networks), and a stop flag.
    Pair { url: String, answer: Arc<Mutex<Option<Result<Vec<crate::pair::Offer>, String>>>>, cancel: Arc<AtomicBool> },
    /// The phone sent these networks: the person at this computer confirms (and may untick some).
    PairConfirm { offers: Vec<(crate::pair::Offer, bool)> },
    Confirm { text: String, op: IpcOp, done: String },
    /// A network's extra relays, one address per line.
    Relays { network: String, text: String },
    /// This computer leaves a network (as a node).
    Leave { network: String },
}

/// `https://relay.example.org/` (or iroh's fully qualified `…org./`) → `relay.example.org`.
fn relay_host(u: &str) -> String {
    u.trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/').trim_end_matches('.').to_string()
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
    /// When the running job ends: start the node in this window (the service was just uninstalled for it).
    then_window: bool,
    /// What the running node (window, terminal or service) says about itself, read every few seconds off
    /// the window thread: `None` while no node answers.
    live: Arc<Mutex<Option<Value>>>,
    live_busy: Arc<AtomicBool>,
    live_at: Option<Instant>,
}

/// How this computer runs its node.
#[derive(Clone, Copy, PartialEq, Debug)]
enum NodeMode {
    Off,
    Window,
    Service,
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
    /// The window's size and place: as last seen (and since when), as last saved.
    geo_seen: Option<(super::Geometry, Instant)>,
    geo_saved: Option<super::Geometry>,
}

fn target(n: &NodeView) -> String {
    format!("{}.{}", n.name, n.network)
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

/// A QR code of `text`, black on white whatever the theme (phone cameras want contrast), with a quiet zone.
/// Modules are whole points, so they stay sharp.
fn qr_image(ui: &mut Ui, text: &str, size: f32) {
    let (n, m) = crate::qr::matrix(text);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 6.0, Color32::WHITE);
    let cell = (size / (n as f32 + 4.0)).floor().max(1.0);
    let origin = rect.center() - egui::vec2(cell * n as f32, cell * n as f32) / 2.0;
    for y in 0..n {
        for x in 0..n {
            if m[y * n + x] {
                let r = egui::Rect::from_min_size(origin + egui::vec2(x as f32 * cell, y as f32 * cell), egui::vec2(cell, cell));
                p.rect_filled(r.expand(0.15), 0.0, Color32::BLACK);
            }
        }
    }
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
            geo_seen: None,
            geo_saved: super::saved_geometry(),
        };
        app.read_node();
        // Without the service, the node runs in this window while it is open (unless turned off).
        if !app.node.installed && !app.node.networks.is_empty() && crate::state::load().is_ok_and(|s| s.node.window_node) {
            app.start_window_node();
        }
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
        // For tests: show the pairing QR at once.
        if std::env::var_os("GANGLI_GUI_PAIR").is_some() {
            app.page = Page::Computer;
            app.start_pairing();
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
            tooltip: format!("gangli: {} of {} machines online", online.len(), self.nodes.len()),
            online,
            open_label: "Open a terminal".into(),
            show_label: "Open gangli".into(),
            quit_label: "Quit".into(),
            machines_label: "Machines".into(),
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
        self.save_geometry();
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
        if visible && self.page == Page::Computer && self.node.live_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(2)) {
            self.poll_node();
        }
        let finished = self.node.job.as_ref().and_then(|j| j.lock().unwrap().take());
        if let Some(out) = finished {
            self.node.job_output = out;
            self.node.job = None;
            self.read_node();
            if std::mem::take(&mut self.node.then_window) && !self.node.installed {
                self.set_window_node(true);
            }
        }
        if let Some(t) = &self.node.thread {
            if t.is_finished() {
                self.node.thread = None;
                self.node.stop = None;
            }
        }
        self.finish_pairing();
    }

    fn on_reply(&mut self, tag: Tag, r: Result<Value, String>) {
        let v = match r {
            Ok(v) => v,
            Err(e) => {
                // Periodic refreshes fail quietly (the next one will say it again); actions say why.
                if tag == Tag::Quiet {
                    super::glog(&e);
                } else if !matches!(tag, Tag::Nodes | Tag::Status | Tag::Tunnels | Tag::Ssh | Tag::Api | Tag::Terms(_)) {
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
                self.modal = Some(Modal::Token { name: s(&v["name"]).into(), token: s(&v["token"]).into(), url: s(&v["url"]).into(), instructions: s(&v["instructions"]).into() });
                self.call(Tag::Api, IpcOp::ApiStatus);
            }
            Tag::Launch => {
                let argv: Vec<String> = serde_json::from_value(v["argv"].clone()).unwrap_or_default();
                match spawn_argv(&argv) {
                    Ok(()) => {
                        let msg = format!("{} started", s(&v["launcher"]));
                        self.toast(msg, false)
                    }
                    Err(e) => self.toast(
                        format!("cannot start {e} (fix its path in the [[launcher]] list of gangli.console)"),
                        true,
                    ),
                }
            }
            Tag::CopyPassword => {
                self.copy = Some(s(&v["password"]).to_string());
                self.toast("Password copied", false);
            }
            Tag::CopySsh => {
                self.copy = Some(format!("ssh -p {} {}@127.0.0.1", v["port"].as_u64().unwrap_or(2222), s(&v["login"])));
                self.toast("ssh command copied", false);
            }
            Tag::Done(msg) => {
                if let Some(list) = v.as_array() {
                    // Update: one result per machine.
                    for r in list {
                        let name = s(&r["node"]).to_string();
                        if r["ok"].as_bool() == Some(true) {
                            self.toast(format!("{name}: {}", "updating"), false);
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
            Tag::Quiet => {}
        }
    }

    /// The window's size and place as the platform layer sees them now (points); saved in `gangli.console`
    /// once they have been still for a second, so the next start opens it the same.
    pub fn window_geometry(&mut self, g: super::Geometry) {
        if self.geo_seen.map(|(s, _)| s) != Some(g) {
            self.geo_seen = Some((g, Instant::now()));
        }
    }

    fn save_geometry(&mut self) {
        let Some((g, at)) = self.geo_seen else { return };
        if at.elapsed() >= Duration::from_secs(1) && self.geo_saved != Some(g) {
            self.geo_saved = Some(g);
            self.call(Tag::Quiet, IpcOp::GuiWindow { x: g.x, y: g.y, w: g.w, h: g.h });
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
                ui.heading("The console could not start");
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
                (Page::Machines, "Machines"),
                (Page::Networks, "Networks"),
                (Page::Tunnels, "Tunnels"),
                (Page::Ssh, "SSH"),
                (Page::Api, "API"),
                (Page::Computer, "This computer"),
            ];
            for (p, label) in pages {
                if ui.selectable_label(self.page == p, label).clicked() {
                    self.page = p;
                    self.polled = None;
                }
            }
            if !self.tabs.is_empty() {
                let label = format!("Terminals ({})", self.tabs.len());
                if ui.selectable_label(self.page == Page::Terminals, label).clicked() {
                    self.page = Page::Terminals;
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Quit").on_hover_text("Close gangli and its console").clicked() {
                    self.quit = true;
                }
                let online = self.nodes.iter().filter(|n| n.online).count();
                let txt = format!("{online} of {} online", self.nodes.len());
                ui.label(RichText::new(txt).weak());
                if !self.client.owns_console() && self.client.broken().is_none() {
                    ui.label(RichText::new("(console of `gangli console run`)").weak());
                }
            });
        });
        ui.add_space(2.0);
    }

    // ----- Machines -----

    fn machines(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.heading("Machines");
            ui.add_space(12.0);
            ui.add(egui::TextEdit::singleline(&mut self.f.filter).hint_text("filter").desired_width(180.0));
        });
        ui.add_space(4.0);
        let nets = self.status["networks"].as_array().map(|a| a.len()).unwrap_or(0);
        if self.nodes.is_empty() {
            ui.add_space(20.0);
            if nets == 0 {
                ui.label("No network yet. Create one in Networks, or add the one you already have with its private code.");
                if ui.button("Go to Networks").clicked() {
                    self.page = Page::Networks;
                }
            } else {
                ui.label("No machine yet. Run gangli on a machine with the network's public code (Networks → Codes), or send it a prepared gangli.");
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
            ui.label(RichText::new("Name").strong());
            ui.label(RichText::new("System").strong());
            ui.label(RichText::new("LAN address").strong());
            ui.label(RichText::new("Link").strong());
            ui.label(RichText::new("Since").strong());
            if many_nets {
                ui.label(RichText::new("Network").strong());
            }
            ui.end_row();
            for n in &nodes {
                let inv = n.inventory.clone().unwrap_or_default();
                let hay = format!("{} {} {} {}", n.name, n.network, inv.os_label(), inv.ips.join(" ")).to_lowercase();
                if !filter.is_empty() && !hay.contains(&filter) {
                    continue;
                }
                super::identicon::avatar(ui, &n.id, 22.0, Some(n.online));
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
                    format!("offline, seen {} ago", human_ago(t.saturating_sub(n.last_seen)))
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
            super::identicon::avatar(ui, &n.id, 34.0, Some(n.online));
            ui.add_space(4.0);
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
        ui.label(RichText::new("Open").strong());
        ui.horizontal(|ui| {
            ui.label("as");
            ui.add(egui::TextEdit::singleline(&mut self.f.user).hint_text("default").desired_width(110.0));
            if !inv.users.is_empty() {
                egui::ComboBox::from_id_salt("users").selected_text("choose").width(80.0).show_ui(ui, |ui| {
                    for u in ["system", "desktop"].iter().map(|s| s.to_string()).chain(inv.users.iter().cloned()) {
                        if ui.selectable_label(self.f.user == u, &u).clicked() {
                            self.f.user = u.clone();
                        }
                    }
                });
            }
        });
        ui.horizontal(|ui| {
            ui.label("persistent terminal");
            ui.add(egui::TextEdit::singleline(&mut self.f.term).hint_text("none").desired_width(110.0));
        });
        let user = (!self.f.user.trim().is_empty()).then(|| self.f.user.trim().to_string());
        let term = (!self.f.term.trim().is_empty()).then(|| self.f.term.trim().to_string());
        let launchers: Vec<String> = self.ssh["launchers"].as_array().map(|a| a.iter().map(|l| s(&l["name"]).to_string()).collect()).unwrap_or_default();
        ui.horizontal_wrapped(|ui| {
            if self.client.console().is_some()
                && ui
                    .add_enabled(n.online, egui::Button::new(RichText::new("Terminal here").strong()))
                    .on_hover_text("A terminal inside this window")
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
            if ui.button("Copy password").clicked() {
                self.call(Tag::CopyPassword, IpcOp::SshPassword { node: tgt.clone(), user: user.clone() });
            }
            if ui.button("Copy ssh command").clicked() {
                self.call(Tag::CopySsh, IpcOp::SshPassword { node: tgt.clone(), user: user.clone() });
            }
        });
        if !n.online {
            ui.label(RichText::new("Offline: it comes back by itself when the machine is on.").weak());
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
                ui.label(RichText::new("Persistent terminals").strong());
                for t in terms {
                    let name = s(&t["name"]).to_string();
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&name).monospace());
                        ui.label(RichText::new(format!("{} · {}", s(&t["user"]), human_ago(now().saturating_sub(t["started"].as_u64().unwrap_or(0))))).weak());
                        let u = Some(s(&t["user"]).to_string()).filter(|u| !u.is_empty());
                        if self.client.console().is_some() && ui.small_button("here").clicked() {
                            self.open_term(&tgt, &n.name, u.clone(), Some(name.clone()));
                        }
                        if ui.small_button("open").clicked() {
                            self.call(Tag::Launch, IpcOp::Launch { node: tgt.clone(), launcher: None, user: u.clone(), term: Some(name.clone()) });
                        }
                        if ui.small_button("close").clicked() {
                            self.call(Tag::Done(format!("terminal {name} closed")), IpcOp::TermKill { node: tgt.clone(), name: name.clone() });
                            self.terms_polled = None;
                        }
                    });
                }
            }
        }

        // Tunnels of this machine.
        ui.add_space(8.0);
        ui.label(RichText::new("Tunnels").strong());
        let mine: Vec<Value> = self.tunnels.iter().filter(|t| s(&t["node"]) == tgt || s(&t["node"]) == n.name).cloned().collect();
        for t in &mine {
            ui.horizontal(|ui| {
                dot(ui, if t["active"].as_bool() == Some(true) { GREEN } else { GREY });
                ui.label(RichText::new(format!("{} → {}", s(&t["listen"]), s(&t["target"]))).monospace());
            });
        }
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.f.tun_target).hint_text("their address, e.g. 192.168.1.20:80").desired_width(200.0));
            ui.add(egui::TextEdit::singleline(&mut self.f.tun_listen).hint_text("local port").desired_width(80.0));
            if ui.button("Add").clicked() {
                self.add_tunnel(tgt.clone(), n.name.clone());
            }
        });

        // What the machine is.
        ui.add_space(8.0);
        ui.label(RichText::new("Machine").strong());
        egui::Grid::new("inv").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
            let mut row = |k: &str, v: String| {
                ui.label(RichText::new(k).weak());
                ui.add(egui::Label::new(v).wrap());
                ui.end_row();
            };
            row("System", format!("{} ({}, {})", inv.os_label(), inv.kernel, inv.arch));
            row("CPU", format!("{} × {}", inv.cpus, inv.cpu));
            row("RAM", format!("{} free of {}", human_bytes(inv.mem_avail), human_bytes(inv.mem_total)));
            for d in &inv.disks {
                row("Disk", format!("{}  {} / {}", d.mount, human_bytes(d.avail), human_bytes(d.total)));
            }
            row("LAN addresses", inv.ips.join(", "));
            row("Users", inv.users.join(", "));
            let access = if inv.full_access { "full" } else { "this user only" };
            row("Runs as", format!("{} ({access})", inv.service_user));
            row("Up for", human_ago(inv.uptime));
            row("gangli", inv.version.clone());
            if let Some(p) = &n.path {
                row("Peer address", p.remote.clone());
            }
            if !inv.relay.is_empty() {
                row("Its relay", relay_host(&inv.relay));
            }
        });

        // Update, forget.
        ui.add_space(8.0);
        ui.label(RichText::new("Update").strong());
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.f.update_from).hint_text("folder with a signed release").desired_width(220.0));
            if ui.add_enabled(n.online && !self.f.update_from.trim().is_empty(), egui::Button::new("Update")).clicked() {
                self.call(Tag::Done(String::new()), IpcOp::Update { nodes: vec![tgt.clone()], from: self.f.update_from.trim().to_string() });
            }
        });
        ui.add_space(8.0);
        if ui.button(RichText::new("Forget this machine").color(RED)).clicked() {
            self.modal = Some(Modal::Confirm {
                text: format!("Forget {}? It is removed from the network; to come back it must be admitted again.", n.name),
                op: IpcOp::Forget { node: tgt.clone() },
                done: format!("{} forgotten", n.name),
            });
        }
    }

    fn add_tunnel(&mut self, node: String, node_name: String) {
        let target = self.f.tun_target.trim().to_string();
        if !target.contains(':') {
            self.toast("Write the address as host:port", true);
            return;
        }
        let listen = self.f.tun_listen.trim().to_string();
        let mut name = self.f.tun_name.trim().to_string();
        if name.is_empty() {
            let port = if listen.is_empty() { target.rsplit(':').next().unwrap_or("").to_string() } else { listen.rsplit(':').next().unwrap_or("").to_string() };
            name = crate::util::slug(&format!("{node_name}-{port}"));
        }
        self.call(
            Tag::Done(format!("tunnel {name} added")),
            IpcOp::TunnelAdd { name, node, target, listen: (!listen.is_empty()).then_some(listen) },
        );
        self.f.tun_target.clear();
        self.f.tun_listen.clear();
        self.f.tun_name.clear();
    }

    // ----- Networks -----

    fn networks(&mut self, ui: &mut Ui) {
        ui.heading("Networks");
        ui.label(RichText::new("A network is a key. Its public code makes a machine a node; its private code makes a console an owner: keep it for yourself.").weak());
        let relay = s(&self.status["relay"]).to_string();
        if !relay.is_empty() {
            ui.label(RichText::new(format!("This console is reachable through the relay {}", relay_host(&relay))).weak());
        }
        ui.add_space(8.0);
        let nets = self.status["networks"].as_array().cloned().unwrap_or_default();
        for n in &nets {
            let name = s(&n["name"]).to_string();
            let relays: Vec<String> = n["relays"].as_array().map(|a| a.iter().map(|u| s(u).to_string()).collect()).unwrap_or_default();
            egui::Frame::group(ui.style()).inner_margin(10).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&name).strong().size(16.0));
                    ui.label(RichText::new(format!("{} of {} machines online · {} consoles", n["online"], n["nodes"], n["consoles"])).weak());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button(RichText::new("Remove").color(RED)).clicked() {
                            self.modal = Some(Modal::Confirm {
                                text: format!("Remove {name} from this console? Without its private code you cannot add it back."),
                                op: IpcOp::NetworkRemove { name: name.clone() },
                                done: format!("{name} removed"),
                            });
                        }
                        if ui.button("Codes").clicked() {
                            self.call(Tag::Codes, IpcOp::NetworkCodes { name: name.clone() });
                        }
                        if ui.button("Relays").on_hover_text("Extra iroh relays for this network, on top of the public n0 ones").clicked() {
                            self.modal = Some(Modal::Relays { network: name.clone(), text: relays.join("\n") });
                        }
                    });
                });
                let extra = if relays.is_empty() { String::new() } else { format!(" + {}", relays.iter().map(|u| relay_host(u)).collect::<Vec<_>>().join(", ")) };
                ui.label(RichText::new(format!("Relays: n0 (public){extra}")).weak());
            });
            ui.add_space(4.0);
        }
        ui.add_space(10.0);
        ui.label(RichText::new("New network").strong());
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.f.net_name).hint_text("name, e.g. family").desired_width(200.0));
            if ui.add_enabled(!self.f.net_name.trim().is_empty(), egui::Button::new("Create")).clicked() {
                self.call(Tag::Codes, IpcOp::NetworkCreate { name: self.f.net_name.trim().to_string() });
                self.f.net_name.clear();
            }
        });
        ui.add_space(6.0);
        ui.label(RichText::new("A network you already own (from another console)").strong());
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.f.add_code).password(true).hint_text("private code (the shorter one)").desired_width(300.0));
            if ui.add_enabled(!self.f.add_code.trim().is_empty(), egui::Button::new("Add")).clicked() {
                self.call(Tag::Done("Network added".into()), IpcOp::NetworkAdd { code: self.f.add_code.trim().to_string(), name: None });
                self.f.add_code.clear();
            }
        });
    }

    // ----- Tunnels -----

    fn tunnels_page(&mut self, ui: &mut Ui) {
        ui.heading("Tunnels");
        ui.label(RichText::new("A local port that reaches an address on a machine's network; saved, and back by itself while gangli is open.").weak());
        ui.add_space(8.0);
        let tunnels = self.tunnels.clone();
        if !tunnels.is_empty() {
            egui::Grid::new("tunnels").striped(true).num_columns(6).spacing([14.0, 8.0]).show(ui, |ui| {
                for h in ["On", "Name", "Machine", "Local", "Their address", ""] {
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
                    if ui.small_button("remove").clicked() {
                        self.call(Tag::Done(format!("tunnel {name} removed")), IpcOp::TunnelRemove { name: name.clone() });
                    }
                    ui.end_row();
                }
            });
        }
        ui.add_space(10.0);
        ui.label(RichText::new("New tunnel").strong());
        ui.horizontal_wrapped(|ui| {
            let names: Vec<(String, String)> = self.nodes.iter().map(|n| (target(n), n.name.clone())).collect();
            let shown = names.iter().find(|(t, _)| *t == self.f.tun_node).map(|(_, n)| n.clone()).unwrap_or_else(|| "machine".into());
            egui::ComboBox::from_id_salt("tun_node").selected_text(shown).show_ui(ui, |ui| {
                for (t, n) in &names {
                    ui.selectable_value(&mut self.f.tun_node, t.clone(), n);
                }
            });
            ui.add(egui::TextEdit::singleline(&mut self.f.tun_target).hint_text("their address host:port").desired_width(200.0));
            ui.add(egui::TextEdit::singleline(&mut self.f.tun_listen).hint_text("local port").desired_width(90.0));
            ui.add(egui::TextEdit::singleline(&mut self.f.tun_name).hint_text("name (optional)").desired_width(130.0));
            if ui.add_enabled(!self.f.tun_node.is_empty(), egui::Button::new("Add")).clicked() {
                let node = self.f.tun_node.clone();
                let name = names.iter().find(|(t, _)| *t == node).map(|(_, n)| n.clone()).unwrap_or_default();
                self.add_tunnel(node, name);
            }
        });
    }

    // ----- SSH -----

    fn ssh_page(&mut self, ui: &mut Ui) {
        ui.heading("SSH gateway");
        let listen = s(&self.ssh["listen"]).to_string();
        ui.label(format!("Any SSH client reaches the machines at {listen} with the login user+machine (e.g. root+nas)."));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Login rules").strong());
            ui.checkbox(&mut self.f.show_passwords, "show passwords");
        });
        ui.label(RichText::new("The most specific rule wins; * means any. With a public key in a rule for * and * (e.g. the one of your Pageant key, from PuTTYgen), PuTTY and WinSCP opened from here log in with it, no password.").weak());
        let rules = self.ssh["logins"].as_array().cloned().unwrap_or_default();
        egui::Grid::new("rules").striped(true).num_columns(6).spacing([14.0, 8.0]).show(ui, |ui| {
            for h in ["#", "Machines", "Users", "Password", "Keys", ""] {
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
                    if ui.small_button("copy").clicked() {
                        self.copy = Some(pw.clone());
                        self.toast("Password copied", false);
                    }
                });
                ui.label(r["keys"].to_string());
                if ui.small_button("remove").clicked() {
                    self.call(Tag::Done("Rule removed".into()), IpcOp::SshLoginRemove { index });
                }
                ui.end_row();
            }
        });
        ui.add_space(8.0);
        ui.label(RichText::new("New rule").strong());
        ui.horizontal_wrapped(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.f.rule_hosts).hint_text("machines (comma), * = all").desired_width(180.0));
            ui.add(egui::TextEdit::singleline(&mut self.f.rule_users).hint_text("users (comma), * = all").desired_width(160.0));
            ui.add(egui::TextEdit::singleline(&mut self.f.rule_password).hint_text("password (empty: generated)").desired_width(170.0));
        });
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.f.rule_key).hint_text("public key ssh-ed25519 AAAA… (optional)").desired_width(420.0));
            if ui.button("Add").clicked() {
                let split = |s: &str| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect::<Vec<_>>();
                let op = IpcOp::SshLoginAdd {
                    hosts: split(&self.f.rule_hosts),
                    users: split(&self.f.rule_users),
                    password: (!self.f.rule_password.trim().is_empty()).then(|| self.f.rule_password.trim().to_string()),
                    key: (!self.f.rule_key.trim().is_empty()).then(|| self.f.rule_key.trim().to_string()),
                };
                self.call(Tag::Done("Rule added".into()), op);
                self.f.rule_hosts.clear();
                self.f.rule_users.clear();
                self.f.rule_password.clear();
                self.f.rule_key.clear();
            }
        });
        ui.add_space(10.0);
        ui.label(RichText::new("Launchers").strong());
        ui.label(RichText::new("Edit them in gangli.console ([[launcher]]): {login} {password} {port} {node} {user} are filled in.").weak());
        for l in self.ssh["launchers"].as_array().cloned().unwrap_or_default() {
            ui.horizontal(|ui| {
                ui.label(RichText::new(s(&l["name"])).strong());
                ui.label(RichText::new(s(&l["command"])).monospace().weak());
            });
        }
    }

    // ----- API -----

    fn api_page(&mut self, ui: &mut Ui) {
        ui.heading("API for programs and AI agents");
        ui.label(RichText::new("A local HTTP API (127.0.0.1 only) while gangli is open. Each program gets its own token, which you can revoke.").weak());
        ui.add_space(8.0);
        let mut on = self.api["enabled"].as_bool() == Some(true);
        ui.horizontal(|ui| {
            if ui.checkbox(&mut on, "Enabled").changed() {
                self.call(Tag::Done(String::new()), if on { IpcOp::ApiEnable } else { IpcOp::ApiDisable });
            }
            ui.label(RichText::new(s(&self.api["url"])).monospace());
        });
        let tokens = self.api["tokens"].as_array().cloned().unwrap_or_default();
        if !tokens.is_empty() {
            ui.add_space(6.0);
            egui::Grid::new("tokens").striped(true).num_columns(4).spacing([14.0, 8.0]).show(ui, |ui| {
                for h in ["Name", "Scope", "Created", ""] {
                    ui.label(RichText::new(h).strong());
                }
                ui.end_row();
                for t in &tokens {
                    let name = s(&t["name"]).to_string();
                    ui.label(&name);
                    ui.label(s(&t["scope"]));
                    ui.label(format!("{} ago", human_ago(now().saturating_sub(t["created"].as_u64().unwrap_or(0)))));
                    if ui.small_button("revoke").clicked() {
                        self.call(Tag::Done(format!("token {name} revoked")), IpcOp::ApiTokenRemove { name: name.clone() });
                    }
                    ui.end_row();
                }
            });
        }
        ui.add_space(10.0);
        ui.label(RichText::new("New token").strong());
        ui.label(RichText::new("read: machines and their details. exec: also commands, jobs, terminals. admin: everything this window and the CLI do but the API tokens: for an AI agent that manages the machines (\"Copy instructions for an AI agent\" tells it how).").weak());
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.f.token_name).hint_text("who uses it, e.g. claude").desired_width(180.0));
            egui::ComboBox::from_id_salt("scope").selected_text(self.f.token_scope.clone()).show_ui(ui, |ui| {
                for sc in ["read", "exec", "admin"] {
                    ui.selectable_value(&mut self.f.token_scope, sc.to_string(), sc);
                }
            });
            if ui.add_enabled(!self.f.token_name.trim().is_empty(), egui::Button::new("Create")).clicked() {
                self.call(Tag::Token, IpcOp::ApiTokenAdd { name: self.f.token_name.trim().to_string(), scope: self.f.token_scope.clone() });
                self.f.token_name.clear();
            }
        });
    }

    // ----- This computer -----

    fn computer(&mut self, ui: &mut Ui) {
        let me = s(&self.status["console_id"]).to_string();
        ui.horizontal(|ui| {
            if !me.is_empty() {
                super::identicon::avatar(ui, &me, 34.0, None);
                ui.add_space(4.0);
            }
            ui.vertical(|ui| {
                ui.heading("This computer");
                ui.label(RichText::new(crate::version_line()).weak());
            });
        });
        ui.add_space(8.0);
        ui.label(RichText::new("Managed from a console (node)").strong());
        let busy = self.node.job.is_some();
        self.node_networks(ui, busy);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!busy, egui::Button::new(RichText::new("Pair with your phone…").strong()))
                .on_hover_text("Shows a QR: scan it with your phone (gangli wallet) and tick the networks — no code to type")
                .clicked()
            {
                self.start_pairing();
            }
            ui.add(egui::TextEdit::singleline(&mut self.f.node_code).hint_text("or a public code (the longer one)").desired_width(300.0));
            if ui.add_enabled(!busy && !self.f.node_code.trim().is_empty(), egui::Button::new("Join")).clicked() {
                let code = self.f.node_code.trim().to_string();
                self.join_codes(vec![code]);
                self.f.node_code.clear();
            }
        });

        // How the node runs: one choice, gangli does the rest (stop, install, uninstall, start).
        ui.add_space(8.0);
        let mode = self.node_mode();
        ui.label(RichText::new("Run the node").strong());
        let mut want = mode;
        ui.add_enabled_ui(!busy, |ui| {
            ui.radio_value(&mut want, NodeMode::Off, "Off").on_hover_text("No console can reach this computer");
            ui.radio_value(&mut want, NodeMode::Window, "While gangli is open")
                .on_hover_text("In this window, as this user: consoles reach this computer while gangli runs (also in the tray)");
            ui.radio_value(&mut want, NodeMode::Service, "Always, as a service")
                .on_hover_text("Starts with the computer, even with nobody logged in; full access. Asks for the administrator once.");
        });
        if want != mode {
            self.set_node_mode(want);
        }
        let (color, line) = match (mode, busy) {
            (_, true) => (AMBER, "Working… confirm the system's request if one appears".to_string()),
            (NodeMode::Service, _) => (
                GREEN,
                format!("Runs as a service{}", self.node.installed_at.as_ref().map(|b| format!(" ({})", b.display())).unwrap_or_default()),
            ),
            (NodeMode::Window, _) => (GREEN, "Runs in this window".to_string()),
            (NodeMode::Off, _) if self.node.networks.is_empty() => (GREY, "Off: join a network first (above)".to_string()),
            (NodeMode::Off, _) => (GREY, "Off: no console can reach this computer".to_string()),
        };
        ui.horizontal(|ui| {
            dot(ui, color);
            ui.label(RichText::new(line).color(color));
        });
        if !self.node.job_output.is_empty() {
            ui.add(egui::Label::new(RichText::new(self.node.job_output.trim()).monospace().weak()).wrap());
        }
        ui.add_space(14.0);
        ui.label(RichText::new("This window").strong());
        let mut on = self.node.autostart;
        if ui
            .checkbox(&mut on, "Start gangli at login, in the tray")
            .on_hover_text("Your machines stay listed and the tunnels on without opening gangli")
            .changed()
        {
            match super::autostart::set(on) {
                Ok(()) => self.node.autostart = on,
                Err(e) => self.toast(format!("{e:#}"), true),
            }
        }
        ui.add_space(14.0);
        ui.label(RichText::new("Files").strong());
        ui.label(RichText::new("Everything gangli keeps is in these files next to it:").weak());
        for p in [crate::paths::node_file(), crate::paths::console_file(), crate::paths::log_file()] {
            ui.label(RichText::new(p.display().to_string()).monospace());
        }
        if ui.button("Show the folder").clicked() {
            reveal(&crate::paths::console_file());
        }
        let _ = exe_dir;
    }

    /// Show a pairing QR and look for the phone's answer in the background (pair.rs).
    fn start_pairing(&mut self) {
        let p = crate::pair::Pairing::new();
        let url = p.url();
        let answer = Arc::new(Mutex::new(None));
        let cancel = Arc::new(AtomicBool::new(false));
        let (a2, c2, wake) = (answer.clone(), cancel.clone(), self.client.waker());
        std::thread::spawn(move || {
            let r = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())
                .and_then(|rt| rt.block_on(p.wait(Duration::from_secs(600), Some(c2))).map_err(|e| format!("{e:#}")));
            *a2.lock().unwrap() = Some(r);
            wake();
        });
        self.modal = Some(Modal::Pair { url, answer, cancel });
    }

    /// The phone answered: ask the person at this computer to confirm the networks it sent.
    fn finish_pairing(&mut self) {
        let Some(Modal::Pair { answer, cancel, .. }) = &self.modal else { return };
        let Some(r) = answer.lock().unwrap().take() else { return };
        let cancelled = cancel.load(Ordering::SeqCst);
        self.modal = None;
        match r {
            Ok(offers) => self.modal = Some(Modal::PairConfirm { offers: offers.into_iter().map(|o| (o, true)).collect() }),
            Err(_) if cancelled => {}
            Err(e) => self.toast(e, true),
        }
    }

    /// Read the running node's status off the window thread.
    fn poll_node(&mut self) {
        self.node.live_at = Some(Instant::now());
        if self.node.live_busy.swap(true, Ordering::SeqCst) {
            return;
        }
        let (live, busy, wake) = (self.node.live.clone(), self.node.live_busy.clone(), self.client.waker());
        std::thread::spawn(move || {
            *live.lock().unwrap() = crate::ipc::node_status();
            busy.store(false, Ordering::SeqCst);
            wake();
        });
    }

    /// The networks this computer is a node of: admitted or not, the consoles connected now, leave.
    fn node_networks(&mut self, ui: &mut Ui, busy: bool) {
        let live = self.node.live.lock().unwrap().clone();
        let t = now();
        let mut leave = None;
        match &live {
            Some(v) => {
                let nets = v["networks"].as_array().cloned().unwrap_or_default();
                if nets.is_empty() {
                    ui.label(RichText::new("Not in any network yet.").weak());
                }
                for n in &nets {
                    let name = s(&n["name"]).to_string();
                    let admitted = n["admitted"].as_bool() == Some(true);
                    let connected = n["connected"].as_array().cloned().unwrap_or_default();
                    egui::Frame::group(ui.style()).inner_margin(8).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            dot(ui, if !admitted { AMBER } else if connected.is_empty() { GREY } else { GREEN });
                            ui.label(RichText::new(&name).strong());
                            if admitted {
                                ui.label(format!("as {}", s(&n["node_name"])));
                            }
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui.add_enabled(!busy, egui::Button::new("Leave")).clicked() {
                                    leave = Some(name.clone());
                                }
                            });
                        });
                        let line = if !admitted {
                            let err = s(&n["error"]);
                            if err.is_empty() { "Waiting for a console of the network to admit it".to_string() } else { format!("Waiting to be admitted: {err}") }
                        } else if connected.is_empty() {
                            match n["last_connected"].as_u64() {
                                Some(at) => format!("No console connected now (last one {} ago)", human_ago(t.saturating_sub(at))),
                                None => "No console connected now: they connect while they are open".to_string(),
                            }
                        } else {
                            let who: Vec<String> = connected
                                .iter()
                                .map(|c| format!("{} (for {})", s(&c["name"]), human_ago(t.saturating_sub(c["since"].as_u64().unwrap_or(t)))))
                                .collect();
                            format!("Connected: {}", who.join(", "))
                        };
                        ui.label(RichText::new(line).weak());
                    });
                }
                let relay = s(&v["relay"]);
                if !relay.is_empty() {
                    ui.label(RichText::new(format!("Reachable through the relay {}", relay_host(relay))).weak());
                }
            }
            None if self.node.installed => {
                ui.label(RichText::new("The service does not answer yet (starting?).").weak());
            }
            None => {
                if self.node.networks.is_empty() {
                    ui.label(RichText::new("Not in any network yet: pair with your phone, or paste a public code.").weak());
                }
                for (net, admitted, name, _) in self.node.networks.clone() {
                    ui.horizontal(|ui| {
                        dot(ui, GREY);
                        ui.label(RichText::new(&net).strong());
                        ui.label(if admitted { format!("as {} · the node is not running", name.unwrap_or_default()) } else { "not admitted yet · the node is not running".into() });
                        if ui.add_enabled(!busy, egui::Button::new("Leave")).clicked() {
                            leave = Some(net.clone());
                        }
                    });
                }
            }
        }
        if let Some(network) = leave {
            self.modal = Some(Modal::Leave { network });
        }
    }

    /// Leave a network: the node tells its consoles, which forget this computer.
    fn leave_network(&mut self, network: String) {
        if self.node.installed {
            self.start_job(self.node.installed_at.clone(), vec!["leave".into(), network]);
            return;
        }
        let r = crate::state::update(|st| {
            let before = st.networks.len();
            st.networks.retain(|m| m.name != network);
            Ok(before != st.networks.len())
        });
        match r {
            Ok(true) => self.toast(format!("Left {network}"), false),
            Ok(false) => self.toast(format!("Not in {network}"), true),
            Err(e) => self.toast(format!("{e:#}"), true),
        }
        self.read_node();
        self.poll_node();
    }

    fn node_mode(&self) -> NodeMode {
        if self.node.installed {
            NodeMode::Service
        } else if self.node.stop.is_some() {
            NodeMode::Window
        } else {
            NodeMode::Off
        }
    }

    /// Go to another mode, doing what it takes: stop the window's node, install or uninstall the service
    /// (one system prompt), start the window's node.
    fn set_node_mode(&mut self, want: NodeMode) {
        match (self.node_mode(), want) {
            (NodeMode::Service, NodeMode::Window) => {
                self.node.then_window = true;
                self.start_job(self.node.installed_at.clone(), vec!["uninstall".into()]);
            }
            (NodeMode::Service, NodeMode::Off) => {
                let _ = crate::state::update(|st| {
                    st.node.window_node = false;
                    Ok(())
                });
                self.start_job(self.node.installed_at.clone(), vec!["uninstall".into()]);
            }
            (_, NodeMode::Service) if self.node.networks.is_empty() => {
                self.toast("Join a network first: pair with your phone, or paste a public code", true);
            }
            (_, NodeMode::Service) => {
                self.stop_window_node();
                self.start_job(None, vec!["install".into()]);
            }
            (_, NodeMode::Window) => self.set_window_node(true),
            (_, NodeMode::Off) => self.set_window_node(false),
        }
    }

    /// Remember whether the window runs the node, and start or stop it now.
    fn set_window_node(&mut self, on: bool) {
        let _ = crate::state::update(|st| {
            st.node.window_node = on;
            Ok(())
        });
        if on {
            if self.node.networks.is_empty() {
                self.toast("Join a network first: pair with your phone, or paste a public code", true);
            } else {
                self.start_window_node();
            }
        } else {
            self.stop_window_node();
        }
    }

    fn start_window_node(&mut self) {
        if self.node.stop.is_some() || self.node.installed {
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        self.node.thread = Some(std::thread::spawn(move || crate::node::run_blocking(s2)));
        self.node.stop = Some(stop);
    }

    fn stop_window_node(&mut self) {
        if let Some(s) = self.node.stop.take() {
            s.store(true, Ordering::SeqCst);
        }
    }

    /// Join these networks and make sure the node runs: the service gets them (one system prompt), else the
    /// node starts in this window.
    fn join_codes(&mut self, codes: Vec<String>) {
        if codes.is_empty() {
            return;
        }
        if self.node.installed {
            let mut args = vec!["join".to_string()];
            args.extend(codes);
            self.start_job(self.node.installed_at.clone(), args);
            self.toast("Confirm the system's request: the service joins the networks", false);
            return;
        }
        let mut joined = vec![];
        for c in &codes {
            match crate::state::add_join(c) {
                Ok(name) => joined.push(name),
                Err(e) => self.toast(format!("{e:#}"), true),
            }
        }
        self.read_node();
        if joined.is_empty() {
            return;
        }
        // A running node picks new memberships up by itself; otherwise start it.
        let off_by_choice = crate::state::load().is_ok_and(|s| !s.node.window_node);
        if self.node.stop.is_none() {
            if off_by_choice {
                let _ = crate::state::update(|st| {
                    st.node.window_node = true;
                    Ok(())
                });
            }
            self.start_window_node();
        }
        self.toast(format!("Joined {}: the node runs; a console of each network admits this computer", joined.join(", ")), false);
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
        // Tabs like a browser's: the active one joins the terminal below it, each has its close button inside
        // (a middle click closes too); they share the width, between 90 and 230 points each.
        ui.spacing_mut().item_spacing.y = 0.0;
        let (mut close, mut activate) = (None, None);
        let tab_w = ((ui.available_width() - 8.0) / self.tabs.len() as f32).clamp(90.0, 230.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for (i, t) in self.tabs.iter().enumerate() {
                let active = i == self.tab_active;
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(tab_w, 32.0), egui::Sense::click());
                let close_rect = egui::Rect::from_center_size(egui::pos2(rect.right() - 16.0, rect.center().y), egui::vec2(20.0, 20.0));
                let close_resp = ui.interact(close_rect, ui.id().with(("tab-close", i)), egui::Sense::click());
                let v = ui.visuals();
                let fill = if active {
                    super::term::BG
                } else if resp.hovered() {
                    v.widgets.hovered.weak_bg_fill
                } else {
                    v.faint_bg_color
                };
                let ink = if active { Color32::from_gray(225) } else { v.text_color() };
                let p = ui.painter();
                p.rect_filled(rect, egui::CornerRadius { nw: 8, ne: 8, sw: 0, se: 0 }, fill);
                let mut title = t.title();
                if t.ended() {
                    title = format!("({title})");
                }
                let text_rect = egui::Rect::from_min_max(rect.min + egui::vec2(10.0, 0.0), egui::pos2(close_rect.left() - 2.0, rect.max.y));
                let galley = p.layout_no_wrap(title.clone(), egui::FontId::proportional(13.5), ink);
                let at = egui::pos2(text_rect.min.x, rect.center().y - galley.size().y / 2.0);
                p.with_clip_rect(text_rect).galley(at, galley, ink);
                if close_resp.hovered() {
                    p.circle_filled(close_rect.center(), 9.0, ink.gamma_multiply(0.25));
                }
                p.text(close_rect.center(), egui::Align2::CENTER_CENTER, "×", egui::FontId::proportional(16.0), ink);
                if close_resp.clicked() || resp.middle_clicked() {
                    close = Some(i);
                } else if resp.clicked() {
                    activate = Some(i);
                }
                resp.on_hover_text(title);
            }
        });
        if let Some(i) = activate {
            self.tab_active = i;
        }
        if let Some(i) = close {
            self.tabs.remove(i);
            if i < self.tab_active || self.tab_active >= self.tabs.len() {
                self.tab_active = self.tab_active.saturating_sub(1);
            }
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
                    ui.heading(format!("Network {network}"));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        qr_image(ui, &crate::pair::network_url(public, network), 150.0);
                        ui.vertical(|ui| {
                            ui.label(RichText::new("Scan with your phone's camera").strong());
                            ui.label(RichText::new(
                                "The gangli wallet opens and keeps this network (its public code only). From the phone you \
                                 can then add machines to it: on a machine, This computer → Pair with your phone.",
                            ).weak());
                        });
                    });
                    ui.add_space(6.0);
                    ui.label(RichText::new("Public code: for the machines to manage").strong());
                    ui.add(egui::Label::new(RichText::new(public.as_str()).monospace()).wrap());
                    ui.horizontal(|ui| {
                        if ui.button("Copy").clicked() {
                            self.copy = Some(public.clone());
                        }
                        if ui.button("Prepare a gangli with it…").on_hover_text("A copy of this program that joins the network by itself: send it to whoever needs help").clicked() {
                            let ext = if cfg!(windows) { ".exe" } else { "" };
                            let out = exe_dir().join(format!("gangli-{network}{ext}"));
                            match std::env::current_exe().map_err(anyhow::Error::from).and_then(|me| crate::embed::pack(&me, &out, public)) {
                                Ok(()) => {
                                    self.toast(format!("ready: {}", out.display()), false);
                                    reveal(&out);
                                }
                                Err(e) => self.toast(format!("{e:#}"), true),
                            }
                        }
                    });
                    ui.add_space(10.0);
                    ui.label(RichText::new("Private code: makes a console an owner of the network").strong().color(AMBER));
                    ui.label(RichText::new("Keep it safe and never put it on a managed machine. It cannot be revoked.").weak());
                    if *show_private {
                        ui.add(egui::Label::new(RichText::new(private.as_str()).monospace()).wrap());
                    }
                    ui.horizontal(|ui| {
                        ui.checkbox(show_private, "show");
                        if ui.button("Copy").clicked() {
                            self.copy = Some(private.clone());
                        }
                    });
                }
                Modal::Token { name, token, url, instructions } => {
                    ui.heading(format!("Token {name}"));
                    ui.label("Shown only now: copy it into the program that uses it.");
                    ui.add(egui::Label::new(RichText::new(token.as_str()).monospace()).wrap());
                    ui.label(RichText::new(format!("{url}   Authorization: Bearer …")).monospace().weak());
                    ui.horizontal(|ui| {
                        if ui.button("Copy the token").clicked() {
                            self.copy = Some(token.clone());
                        }
                        if ui
                            .button("Copy instructions for an AI agent")
                            .on_hover_text("The token, the address and how to list, reach and command the machines: paste it into the agent's prompt or a file it reads")
                            .clicked()
                        {
                            self.copy = Some(instructions.clone());
                            self.toast("Instructions copied", false);
                        }
                    });
                    if self.api["enabled"].as_bool() != Some(true) {
                        ui.label(RichText::new("The API is off: turn it on to use the token.").color(AMBER));
                    }
                }
                Modal::Confirm { text, .. } => {
                    ui.label(text.as_str());
                }
                Modal::Leave { network } => {
                    ui.heading(format!("Leave {network}?"));
                    ui.label(
                        "This computer stops being a machine of that network: its consoles forget it and can no longer \
                         reach it. To come back it must join again (phone or public code).",
                    );
                }
                Modal::Relays { network, text } => {
                    ui.heading(format!("Relays of {network}"));
                    ui.label(RichText::new(
                        "When two machines cannot reach each other directly, their traffic goes through a relay (end-to-end \
                         encrypted: the relay cannot read it). The public n0 relays are always used; add your own here, one \
                         address per line. This console uses them, sends them to the network's machines and to its other \
                         consoles.",
                    ).weak());
                    ui.add_space(6.0);
                    ui.add(egui::TextEdit::multiline(text).hint_text("https://relay.example.org").desired_rows(4).desired_width(f32::INFINITY).font(egui::TextStyle::Monospace));
                }
                Modal::PairConfirm { offers } => {
                    ui.heading("Your phone sent networks");
                    ui.label("Join this computer to:");
                    ui.add_space(4.0);
                    for (o, on) in offers.iter_mut() {
                        ui.checkbox(on, RichText::new(o.label()).strong());
                    }
                    ui.add_space(4.0);
                    ui.label(RichText::new("The node then runs and a console of each network admits this computer. Only join networks you expect.").weak());
                }
                Modal::Pair { url, .. } => {
                    ui.heading("Join networks from your phone");
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        qr_image(ui, url, 220.0);
                        ui.vertical(|ui| {
                            ui.label("1. Scan this with your phone's camera: the gangli wallet opens.");
                            ui.label("2. Tick the networks for this computer and tap Add.");
                            ui.label("3. Confirm here; a console of each network admits this computer.");
                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(RichText::new("Waiting for the phone…").weak());
                            });
                        });
                    });
                    ui.add(egui::Label::new(RichText::new(url.as_str()).monospace().small().weak()).wrap());
                }
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if let Modal::Confirm { op, done, .. } = &m {
                    if ui.button(RichText::new("Yes").color(RED)).clicked() {
                        self.call(Tag::Done(done.clone()), op.clone());
                        keep = false;
                    }
                    if ui.button("Cancel").clicked() {
                        keep = false;
                    }
                } else if let Modal::Leave { network } = &m {
                    if ui.button(RichText::new("Leave").color(RED)).clicked() {
                        self.leave_network(network.clone());
                        keep = false;
                    }
                    if ui.button("Cancel").clicked() {
                        keep = false;
                    }
                } else if let Modal::PairConfirm { offers } = &m {
                    let codes: Vec<String> = offers.iter().filter(|(_, on)| *on).map(|(o, _)| o.code.clone()).collect();
                    if ui.add_enabled(!codes.is_empty(), egui::Button::new(RichText::new("Join").strong())).clicked() {
                        self.join_codes(codes);
                        keep = false;
                    }
                    if ui.button("Cancel").clicked() {
                        keep = false;
                    }
                } else if let Modal::Relays { network, text } = &m {
                    if ui.button(RichText::new("Save").strong()).clicked() {
                        let urls: Vec<String> = text.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
                        let bad: Vec<&String> = urls.iter().filter(|u| crate::util::relay_url(u).is_err()).collect();
                        if let Some(b) = bad.first() {
                            self.toast(format!("{b}: not a relay address (https://…)"), true);
                        } else {
                            let done = format!("Relays of {network} saved and sent to its machines");
                            self.call(Tag::Done(done), IpcOp::NetworkRelays { name: network.clone(), set: Some(urls) });
                            keep = false;
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        keep = false;
                    }
                } else if ui.button("Close").clicked() {
                    keep = false;
                }
            });
        });
        if keep && !ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.modal = Some(m);
        } else if let Modal::Pair { cancel, .. } = &m {
            // Closing the dialog stops looking for the phone.
            cancel.store(true, Ordering::SeqCst);
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
                .on_hover_text("Through a relay server: slower. Direct links need UDP to get through.");
        }
        (Some(p), true) => {
            ui.label(RichText::new(format!("{} {} ms", "direct", p.rtt_ms)).color(GREEN));
        }
        _ => {
            ui.label("");
        }
    }
}
