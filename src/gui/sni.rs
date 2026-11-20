//! The Linux tray icon: a StatusNotifierItem over D-Bus (ksni, pure Rust), shown by KDE, GNOME with the
//! AppIndicator extension, XFCE, Cinnamon and most panels. Without a tray host the window just quits on close.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ksni::menu::{StandardItem, SubMenu};
use ksni::{MenuItem, TrayMethods};

use super::{TrayAction, TrayModel};

type Send = Arc<dyn Fn(TrayAction) + std::marker::Send + Sync>;

struct GTray {
    model: TrayModel,
    send: Send,
}

fn argb(size: u32) -> ksni::Icon {
    let rgba = super::icon::rgba(size);
    let mut data = Vec::with_capacity(rgba.len());
    for p in rgba.chunks_exact(4) {
        data.extend_from_slice(&[p[3], p[0], p[1], p[2]]);
    }
    ksni::Icon { width: size as i32, height: size as i32, data }
}

impl ksni::Tray for GTray {
    fn id(&self) -> String {
        "gangli".into()
    }

    fn title(&self) -> String {
        "gangli".into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![argb(22), argb(32), argb(48)]
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip { title: self.model.tooltip.clone(), ..Default::default() }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        (self.send)(TrayAction::Show);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let open: Vec<MenuItem<Self>> = self
            .model
            .online
            .iter()
            .map(|name| {
                let n = name.clone();
                StandardItem { label: name.clone(), activate: Box::new(move |t: &mut Self| (t.send)(TrayAction::Open(n.clone()))), ..Default::default() }.into()
            })
            .collect();
        vec![
            StandardItem { label: self.model.show_label.clone(), activate: Box::new(|t: &mut Self| (t.send)(TrayAction::Show)), ..Default::default() }.into(),
            SubMenu { label: self.model.open_label.clone(), enabled: !open.is_empty(), submenu: open, ..Default::default() }.into(),
            MenuItem::Separator,
            StandardItem { label: self.model.quit_label.clone(), activate: Box::new(|t: &mut Self| (t.send)(TrayAction::Quit)), ..Default::default() }.into(),
        ]
    }
}

/// The tray, on its own thread. `available` turns true once a tray host shows it. Returns the channel that
/// updates its content.
pub fn spawn(model: TrayModel, send: Send, available: Arc<AtomicBool>) -> tokio::sync::mpsc::UnboundedSender<TrayModel> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<TrayModel>();
    std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
        rt.block_on(async move {
            let handle = match (GTray { model, send }).spawn().await {
                Ok(h) => h,
                Err(e) => {
                    super::glog(&format!("no tray icon ({e})"));
                    return;
                }
            };
            available.store(true, Ordering::SeqCst);
            while let Some(m) = rx.recv().await {
                handle.update(|t| t.model = m).await;
            }
            handle.shutdown().await;
        });
    });
    tx
}
