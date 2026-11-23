//! A `StatusNotifierItem` over D-Bus (`ksni` on the daemon's tokio runtime): no GTK, and the
//! tray host draws the icon and the menu.

use std::process::ExitCode;
use std::sync::Arc;

use ksni::menu::{MenuItem, StandardItem};
use ksni::{Handle, ToolTip, TrayMethods};
use nsv_domain::helper::Rect;
use tokio::sync::mpsc;

use super::{Action, Entry, Icon, Look, MENU, enabled, rgba};
use crate::log;

/// Nothing on Linux: the tray joins the daemon's runtime when [`crate::run`] starts.
#[derive(Debug)]
pub struct TrayLink(());

impl TrayLink {
    /// The autostart entry passes `--background` instead.
    #[expect(
        clippy::unused_self,
        reason = "the macOS twin reads the launch's Apple event"
    )]
    pub(crate) fn login_launch(&self) -> bool {
        false
    }
}

/// Runs `daemon` on this thread and exits with its code. macOS needs the main thread for the
/// menu bar item; Linux runs the tray on the daemon's runtime.
pub fn run_desktop(daemon: impl FnOnce(TrayLink) -> u8) -> ExitCode {
    ExitCode::from(daemon(TrayLink(())))
}

/// The tray's ARGB32 pixmaps, decoded once.
struct Pixmaps {
    ball: ksni::Icon,
    stopped: ksni::Icon,
}

impl Pixmaps {
    fn decode() -> Result<Self, png::DecodingError> {
        let pixmap = |bytes: &[u8]| -> Result<ksni::Icon, png::DecodingError> {
            let (mut data, width, height) = rgba(bytes)?;
            // RGBA to ARGB in network byte order.
            for pixel in data.as_chunks_mut::<4>().0 {
                pixel.rotate_right(1);
            }
            Ok(ksni::Icon {
                width: i32::try_from(width).unwrap_or(0),
                height: i32::try_from(height).unwrap_or(0),
                data,
            })
        };
        Ok(Self {
            ball: pixmap(include_bytes!("../../../../public/tray/linux-ball.png"))?,
            stopped: pixmap(include_bytes!("../../../../public/tray/linux-stopped.png"))?,
        })
    }
}

struct Item {
    look: Look,
    updates: bool,
    pixmaps: Arc<Pixmaps>,
    actions: mpsc::Sender<Action>,
}

impl Item {
    fn send(&self, action: Action) {
        if self.actions.try_send(action).is_err() {
            log(format_args!(
                "dropped a tray click: the shell is busy or gone"
            ));
        }
    }
}

impl ksni::Tray for Item {
    fn id(&self) -> String {
        "nonstopvibin".to_owned()
    }

    fn title(&self) -> String {
        "NonstopVibin".to_owned()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![match self.look.icon {
            Icon::Ball => self.pixmaps.ball.clone(),
            Icon::Stopped => self.pixmaps.stopped.clone(),
        }]
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: self.look.tooltip.clone(),
            ..ToolTip::default()
        }
    }

    /// A left click. Hosts that know where it happened pass the point; others pass 0, 0, and
    /// the popup then sits in the bottom-left corner, as Electron's did on Linux.
    fn activate(&mut self, x: i32, y: i32) {
        self.send(Action::ToggleQuota(Some(Rect {
            x,
            y,
            width: 0,
            height: 0,
        })));
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        MENU.iter()
            .map(|entry| match *entry {
                Entry::Item { label, action } => StandardItem {
                    label: label.to_owned(),
                    enabled: enabled(action, self.updates),
                    activate: Box::new(move |item: &mut Self| {
                        if enabled(action, item.updates) {
                            item.send(action);
                        }
                    }),
                    ..StandardItem::default()
                }
                .into(),
                Entry::Separator => MenuItem::Separator,
            })
            .collect()
    }
}

pub(crate) struct Tray {
    handle: Option<Handle<Item>>,
    actions: mpsc::Receiver<Action>,
}

impl Tray {
    /// Registers the item with the `StatusNotifierWatcher`. Without a session bus or a tray host
    /// the shell runs on without a tray, as Electron did.
    /// `updates` enables "Check for updates…".
    pub(crate) async fn start(_link: TrayLink, look: Look, updates: bool) -> Self {
        let (sender, actions) = mpsc::channel(8);
        let handle = match Pixmaps::decode() {
            Ok(pixmaps) => {
                let item = Item {
                    look,
                    updates,
                    pixmaps: Arc::new(pixmaps),
                    actions: sender,
                };
                item.spawn()
                    .await
                    .inspect_err(|error| log(format_args!("no tray: {error}")))
                    .ok()
            }
            Err(error) => {
                log(format_args!("no tray: the icons do not decode: {error}"));
                None
            }
        };
        Self { handle, actions }
    }

    /// `None` once no tray can report anything.
    pub(crate) async fn next(&mut self) -> Option<Action> {
        self.actions.recv().await
    }

    pub(crate) async fn show(&self, look: Look) {
        if let Some(handle) = &self.handle {
            handle.update(|item| item.look = look).await;
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        if let Some(handle) = &self.handle {
            // The service task removes the item from the bus once it sees this.
            drop(handle.shutdown());
        }
    }
}
