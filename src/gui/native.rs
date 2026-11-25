//! Windows and macOS: the window on eframe (winit + OpenGL), the tray icon on tray-icon. Closing the window
//! hides it in the tray; the tray's menu opens it again, opens terminals on online machines, or quits.

use std::collections::HashMap;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use anyhow::{anyhow, Result};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use super::client::Client;
use super::{App, TrayAction, TrayModel};

pub fn run(client: Client, hidden: bool) -> Result<()> {
    let icon = egui::IconData { rgba: super::icon::rgba(64), width: 64, height: 64 };
    let saved = super::saved_geometry();
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("gangli")
        .with_app_id("gangli")
        .with_min_inner_size([super::MIN_SIZE.0, super::MIN_SIZE.1])
        .with_icon(Arc::new(icon))
        .with_visible(!hidden);
    viewport = match saved {
        Some(g) => viewport.with_inner_size([g.w, g.h]).with_position([g.x, g.y]),
        // Small enough for any screen until the first frame knows the screen's size (see `fit`).
        None => viewport.with_inner_size([900.0, 580.0]),
    };
    #[allow(unused_mut)]
    let mut options = eframe::NativeOptions { viewport, ..Default::default() };
    #[cfg(windows)]
    {
        options.wgpu_options = wgpu_setup();
    }
    eframe::run_native(
        "gangli",
        options,
        Box::new(move |cc| {
            let app = App::new(client);
            let ctx = cc.egui_ctx.clone();
            app.set_waker(Arc::new(move || ctx.request_repaint()));
            let mut n = Native::new(app, &cc.egui_ctx);
            n.fit = saved.is_none();
            // Hidden only when the tray can bring it back.
            if hidden && n.tray.is_some() {
                n.visible = false;
            } else if hidden {
                cc.egui_ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            }
            Ok(Box::new(n))
        }),
    )
    .map_err(|e| anyhow!("{e}"))
}

/// Windows draws with Direct3D 12 (wgpu): the graphics card when it has a Direct3D 12 driver, otherwise WARP,
/// the software renderer every Windows 10 and 11 has. OpenGL is not an option there: old cards ship
/// Windows 7 era drivers (OpenGL 2.1) that draw egui all wrong, and machines without a driver (virtual ones,
/// the CI runners) have none at all.
#[cfg(windows)]
fn wgpu_setup() -> eframe::WgpuConfiguration {
    use eframe::egui_wgpu;
    use eframe::wgpu;
    let mut setup = egui_wgpu::WgpuSetupCreateNew::without_display_handle();
    setup.instance_descriptor.backends = wgpu::Backends::DX12;
    // A real card first (the integrated one before a power-hungry discrete one), WARP last.
    let pick: egui_wgpu::NativeAdapterSelectorMethod = Arc::new(|adapters: &[wgpu::Adapter], surface: Option<&wgpu::Surface<'_>>| {
        let rank = |a: &wgpu::Adapter| match a.get_info().device_type {
            wgpu::DeviceType::IntegratedGpu => 0,
            wgpu::DeviceType::DiscreteGpu => 1,
            wgpu::DeviceType::VirtualGpu | wgpu::DeviceType::Other => 2,
            wgpu::DeviceType::Cpu => 3,
        };
        adapters
            .iter()
            .filter(|a| surface.is_none_or(|s| a.is_surface_supported(s)))
            .min_by_key(|a| rank(a))
            .cloned()
            .ok_or_else(|| "no Direct3D 12 adapter, not even WARP".to_string())
    });
    setup.native_adapter_selector = Some(pick);
    egui_wgpu::WgpuConfiguration { wgpu_setup: egui_wgpu::WgpuSetup::CreateNew(setup), ..Default::default() }
}

struct Native {
    app: App,
    tray: Option<TrayIcon>,
    model: TrayModel,
    menu_rx: mpsc::Receiver<MenuId>,
    click_rx: mpsc::Receiver<TrayIconEvent>,
    actions: HashMap<MenuId, TrayAction>,
    visible: bool,
    quitting: bool,
    /// Test screenshot: file, delay, requested, written.
    shot: Option<(String, Duration)>,
    started: std::time::Instant,
    shot_sent: bool,
    shot_done: Arc<std::sync::atomic::AtomicBool>,
    /// `GANGLI_GUI_TEST_TRAY`: close the window at 2 s (it must go to the tray), open it from the tray at 5 s.
    test_tray: u8,
    /// No saved size: size and center the window for the screen once its size is known.
    fit: bool,
}

impl Native {
    fn new(app: App, ctx: &egui::Context) -> Native {
        let (mtx, menu_rx) = mpsc::channel();
        let (ctx1, ctx2) = (ctx.clone(), ctx.clone());
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            let _ = mtx.send(e.id);
            ctx1.request_repaint();
        }));
        let (ctx_tx, click_rx) = mpsc::channel();
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
            let _ = ctx_tx.send(e);
            ctx2.request_repaint();
        }));
        let model = app.tray_model();
        let (menu, actions) = build_menu(&model);
        let tray = tray_icon::Icon::from_rgba(super::icon::rgba(32), 32, 32).ok().and_then(|icon| {
            TrayIconBuilder::new()
                .with_icon(icon)
                .with_tooltip(&model.tooltip)
                .with_menu(Box::new(menu))
                .with_menu_on_left_click(cfg!(target_os = "macos"))
                .build()
                .map_err(|e| super::glog(&format!("no tray icon ({e})")))
                .ok()
        });
        Native {
            app,
            tray,
            model,
            menu_rx,
            click_rx,
            actions,
            visible: true,
            quitting: false,
            shot: super::png::screenshot_request(),
            started: std::time::Instant::now(),
            shot_sent: false,
            shot_done: Arc::default(),
            test_tray: u8::from(std::env::var_os("GANGLI_GUI_TEST_TRAY").is_some()),
            fit: false,
        }
    }

    /// First start: a size for this screen, centered. Then: tell the app the size and place to remember
    /// (not while minimized, maximized or full screen: the next start opens it normal).
    fn geometry(&mut self, ctx: &egui::Context) {
        let (monitor, outer, inner, minimized, maximized, fullscreen) = ctx.input(|i| {
            let v = i.viewport();
            (v.monitor_size, v.outer_rect, v.inner_rect, v.minimized, v.maximized, v.fullscreen)
        });
        if self.fit {
            let (Some(m), Some(o), Some(inr)) = (monitor, outer, inner) else { return };
            self.fit = false;
            let (w, h) = super::default_size(m.x, m.y);
            // The frame around the inner area (title bar, borders) stays as it is.
            let (fw, fh) = (o.width() - inr.width(), o.height() - inr.height());
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(w, h)));
            let pos = egui::pos2(((m.x - w - fw) / 2.0).max(0.0), ((m.y - h - fh) / 2.0).max(0.0));
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
            return;
        }
        if !self.visible || minimized == Some(true) || maximized == Some(true) || fullscreen == Some(true) {
            return;
        }
        if let (Some(o), Some(inr)) = (outer, inner) {
            self.app.window_geometry(super::Geometry { x: o.min.x, y: o.min.y, w: inr.width(), h: inr.height() });
        }
    }

    fn show(&mut self, ctx: &egui::Context) {
        self.visible = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }
}

fn build_menu(m: &TrayModel) -> (Menu, HashMap<MenuId, TrayAction>) {
    let menu = Menu::new();
    let mut actions = HashMap::new();
    let show = MenuItem::new(&m.show_label, true, None);
    actions.insert(show.id().clone(), TrayAction::Show);
    let _ = menu.append(&show);
    let open = Submenu::new(&m.open_label, !m.online.is_empty());
    for name in &m.online {
        let item = MenuItem::new(name, true, None);
        actions.insert(item.id().clone(), TrayAction::Open(name.clone()));
        let _ = open.append(&item);
    }
    let _ = menu.append(&open);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let quit = MenuItem::new(&m.quit_label, true, None);
    actions.insert(quit.id().clone(), TrayAction::Quit);
    let _ = menu.append(&quit);
    (menu, actions)
}

impl eframe::App for Native {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.test_tray == 1 && self.started.elapsed() >= Duration::from_secs(2) {
            self.test_tray = 2;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else if self.test_tray == 2 && self.started.elapsed() >= Duration::from_secs(5) {
            self.test_tray = 3;
            super::glog(&format!("test: window visible before reopening: {} (system: {:?})", self.visible, os_visible()));
            self.app.tray(TrayAction::Show);
        } else if self.test_tray == 3 && self.started.elapsed() >= Duration::from_secs(7) {
            self.test_tray = 4;
            super::glog(&format!("test: window visible after reopening: {} (system: {:?})", self.visible, os_visible()));
        }
        if self.test_tray > 0 && self.test_tray < 4 {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting {
            if self.tray.is_some() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                self.visible = false;
            } else {
                self.app.tray(TrayAction::Quit);
            }
        }
        while let Ok(id) = self.menu_rx.try_recv() {
            if let Some(a) = self.actions.get(&id).cloned() {
                self.app.tray(a);
            }
        }
        while let Ok(e) = self.click_rx.try_recv() {
            match e {
                TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } | TrayIconEvent::DoubleClick { .. } => {
                    if !cfg!(target_os = "macos") {
                        self.app.tray(TrayAction::Show);
                    }
                }
                _ => {}
            }
        }
        if let Some((path, after)) = self.shot.clone() {
            use std::sync::atomic::Ordering;
            if self.shot_done.load(Ordering::SeqCst) {
                self.app.tray(TrayAction::Quit);
            } else if !self.shot_sent && self.started.elapsed() >= after {
                self.shot_sent = true;
                let (done, ctx2) = (self.shot_done.clone(), ctx.clone());
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::ScreenshotCallback::new(move |img| {
                    let rgb: Vec<u8> = img.pixels.iter().flat_map(|c| [c.r(), c.g(), c.b()]).collect();
                    let _ = std::fs::write(&path, super::png::encode(img.size[0], img.size[1], &rgb));
                    super::glog(&format!("screenshot saved to {path}"));
                    done.store(true, Ordering::SeqCst);
                    ctx2.request_repaint();
                })));
            }
            ctx.request_repaint_after(Duration::from_millis(300));
        }
        self.geometry(ctx);
        self.app.logic(self.visible);
        if self.app.take_show() {
            self.show(ctx);
        }
        if self.app.quit_requested() && !self.quitting {
            self.quitting = true;
            self.app.shutdown();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        let model = self.app.tray_model();
        if model != self.model {
            if let Some(t) = &self.tray {
                let (menu, actions) = build_menu(&model);
                t.set_menu(Some(Box::new(menu)));
                let _ = t.set_tooltip(Some(&model.tooltip));
                self.actions = actions;
            }
            self.model = model;
        }
        if !self.visible {
            // Keep the answers and the tray's machine list flowing while hidden.
            ctx.request_repaint_after(Duration::from_secs(2));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.app.ui(ui);
    }

    #[cfg(target_os = "macos")]
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.quit_once();
    }

    #[cfg(windows)]
    fn on_exit(&mut self) {
        self.quit_once();
    }
}

impl Native {
    fn quit_once(&mut self) {
        if !self.quitting {
            self.quitting = true;
            self.app.shutdown();
        }
    }
}

/// Whether the system shows the gangli window (the tray test checks the real state, not ours).
fn os_visible() -> Option<bool> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, IsWindowVisible};
        let title: Vec<u16> = "gangli".encode_utf16().chain(Some(0)).collect();
        let h = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
        if h.is_null() {
            return None;
        }
        Some(unsafe { IsWindowVisible(h) } != 0)
    }
    #[cfg(not(windows))]
    None
}
