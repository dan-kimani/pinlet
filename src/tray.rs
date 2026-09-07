//! System tray integration via ksni (StatusNotifierItem over DBus).
//!
//! ksni runs the tray on its own thread; menu actions funnel into
//! the GTK main loop through a [`glib::Sender`] carrying [`Msg`]s.
//! The menu itself is built from a [`TraySnapshot`] that the app
//! core refreshes whenever notes change.

use std::sync::mpsc::Sender as MsgSender;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Local, Utc};
use gtk4::gdk_pixbuf::prelude::*;
use gtk4::gdk_pixbuf::{InterpType, PixbufLoader};
use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::{StandardItem, SubMenu};
use ksni::{Icon, MenuItem, Tray};
use uuid::Uuid;

use crate::messages::Msg;

/// The app icon, rendered to ARGB32 for the tray. Embedded so the tray
/// shows the real icon even in a dev build where the icon is not yet
/// installed into the system icon theme.
const ICON_SVG: &[u8] = include_bytes!("../assets/org.pinlet.Pinlet.svg");

/// Standard tray icon sizes to offer; the host picks the one it needs.
const ICON_SIZES: [i32; 4] = [22, 24, 32, 48];

/// Rasterize the app icon at `size` pixels as an ARGB32 pixmap
/// (network byte order — `[A, R, G, B]` per pixel).
fn render_icon(size: i32) -> Option<Icon> {
    let loader = PixbufLoader::new();
    loader.write(ICON_SVG).ok()?;
    loader.close().ok()?;
    let pixbuf = loader.pixbuf()?;
    let scaled = pixbuf.scale_simple(size, size, InterpType::Bilinear)?;
    if scaled.n_channels() != 4 {
        return None;
    }

    let raw = scaled.read_pixel_bytes();
    let raw = raw.as_ref();
    let rowstride = scaled.rowstride() as usize;
    let width = scaled.width() as usize;
    let height = scaled.height() as usize;

    // gdk-pixbuf stores RGBA; StatusNotifierItem wants ARGB32, so rotate
    // each pixel's four bytes right by one (`[R, G, B, A]` → `[A, R, G, B]`).
    let mut data = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        let row = &raw[y * rowstride..y * rowstride + width * 4];
        for pixel in row.chunks_exact(4) {
            data.extend_from_slice(&[pixel[3], pixel[0], pixel[1], pixel[2]]);
        }
    }

    Some(Icon {
        width: size,
        height: size,
        data,
    })
}

/// Install the app icon into the user's icon theme so it resolves by name.
///
/// The Ubuntu appindicator extension resolves tray icons through
/// `Gtk.IconTheme` using `IconName`, ignoring `IconPixmap`, so the SVG has
/// to live where the theme can find it. Best-effort and idempotent: failures
/// are logged, never fatal.
pub fn install_icon() {
    let hicolor = gtk4::glib::user_data_dir().join("icons/hicolor");
    let dest = hicolor.join("scalable/apps/org.pinlet.Pinlet.svg");
    if let Some(parent) = dest.parent() {
        if let Err(err) = std::fs::create_dir_all(parent)
            .and_then(|_| std::fs::write(&dest, ICON_SVG))
        {
            eprintln!("failed to install app icon: {err}");
            return;
        }
    }
    // Drop the theme cache so GTK re-scans and notices the new icon.
    let _ = std::fs::remove_file(hicolor.join(".icon-theme.cache"));
}

/// A short human label for how far in the future `due` is.
fn format_due(due: DateTime<Utc>) -> String {
    let minutes = (due - Utc::now()).num_minutes();
    if minutes < 1 {
        "now".to_owned()
    } else if minutes < 60 {
        format!("in {minutes}m")
    } else if minutes < 60 * 24 {
        format!("in {}h", minutes / 60)
    } else if minutes < 60 * 24 * 7 {
        format!("in {}d", minutes / (60 * 24))
    } else {
        due.with_timezone(&Local).format("%b %e, %H:%M").to_string()
    }
}

/// A due reminder shown in the tray menu (top 5 upcoming).
#[derive(Debug, Clone)]
pub struct DueReminder {
    /// Owning note.
    pub note: Uuid,
    /// Note title for display.
    pub title: String,
    /// Due time.
    pub due: DateTime<Utc>,
}

/// Snapshot of everything the tray menu needs; written by the app
/// core, read by the tray thread.
#[derive(Debug, Clone, Default)]
pub struct TraySnapshot {
    /// Upcoming reminders, sorted by due time.
    pub upcoming: Vec<DueReminder>,
}

/// The ksni tray handle type.
pub type TrayHandle = Handle<PinletTray>;

/// The system tray item.
pub struct PinletTray {
    /// Message channel into the GTK main loop.
    tx: MsgSender<Msg>,
    /// Menu snapshot shared with the app core.
    snapshot: Arc<Mutex<TraySnapshot>>,
    /// Pre-rendered app icon at several tray sizes.
    icon: Vec<Icon>,
}

impl PinletTray {
    /// Build a tray that sends actions over `tx` and reads menu data
    /// from `snapshot`. The app icon is rasterized here on the main
    /// thread; the tray thread only reads the finished pixmaps.
    pub fn new(tx: MsgSender<Msg>, snapshot: Arc<Mutex<TraySnapshot>>) -> Self {
        install_icon();
        let icon = ICON_SIZES
            .iter()
            .filter_map(|&size| render_icon(size))
            .collect();
        Self { tx, snapshot, icon }
    }

    /// A standard menu item that sends one message.
    fn item(label: &str, icon: &str, msg: Msg, tx: &MsgSender<Msg>) -> MenuItem<Self> {
        let tx = tx.clone();
        MenuItem::Standard(StandardItem {
            label: label.to_owned(),
            icon_name: icon.to_owned(),
            activate: Box::new(move |_| {
                let _ = tx.send(msg.clone());
            }),
            ..Default::default()
        })
    }

    /// A reminder entry: its due label as a submenu carrying open/snooze
    /// actions.
    fn reminder_item(reminder: &DueReminder, tx: &MsgSender<Msg>) -> MenuItem<Self> {
        let label = format!("{} — {}", reminder.title, format_due(reminder.due));
        let snooze = |minutes| Msg::Snooze {
            note: reminder.note,
            due: reminder.due,
            minutes,
        };
        MenuItem::SubMenu(SubMenu {
            label,
            icon_name: "alarm-symbolic".to_owned(),
            submenu: vec![
                Self::item("Open note", "document-open-symbolic", Msg::FocusNote(reminder.note), tx),
                MenuItem::Separator,
                Self::item("Snooze 10 minutes", "alarm-symbolic", snooze(10), tx),
                Self::item("Snooze 1 hour", "alarm-symbolic", snooze(60), tx),
                Self::item("Snooze 1 day", "alarm-symbolic", snooze(1440), tx),
            ],
            ..Default::default()
        })
    }
}

impl Tray for PinletTray {
    /// Left click opens the menu instead of calling `activate`.
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        "pinlet".to_owned()
    }

    fn icon_name(&self) -> String {
        // Theme-name fallback for when the icon is installed; the pixmap
        // below is what actually shows in a dev build.
        "org.pinlet.Pinlet".to_owned()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        self.icon.clone()
    }

    fn title(&self) -> String {
        "Pinlet".to_owned()
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.send(Msg::ToggleAll);
    }

    /// Overriding this signals ksni to rebuild the menu on every show, so
    /// the reminder list stays current. (The snapshot itself is refreshed
    /// by the app core whenever notes or reminders change.)
    fn menu_about_to_show(&mut self) {}

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items = Vec::new();

        items.push(Self::item("New note", "list-add-symbolic", Msg::NewNote, &self.tx));
        items.push(Self::item("Show / hide all", "view-restore-symbolic", Msg::ToggleAll, &self.tx));

        // Dynamic section: top 5 upcoming reminders (spec §3.3).
        let snapshot = self.snapshot.lock().expect("tray snapshot poisoned").clone();
        if !snapshot.upcoming.is_empty() {
            items.push(MenuItem::Separator);
            for reminder in &snapshot.upcoming {
                items.push(Self::reminder_item(reminder, &self.tx));
            }
        }

        items.push(MenuItem::Separator);
        items.push(Self::item(
            "Preferences",
            "preferences-system-symbolic",
            Msg::OpenSettings,
            &self.tx,
        ));
        items.push(Self::item("Quit", "application-exit-symbolic", Msg::Quit, &self.tx));
        items
    }
}

/// Spawn the tray on its own thread. Best effort: environments
/// without a StatusNotifier host return an error and the app keeps
/// running tray-less.
pub fn spawn(tray: PinletTray) -> Result<TrayHandle, ksni::Error> {
    tray.spawn()
}
