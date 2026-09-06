//! System tray integration via ksni (StatusNotifierItem over DBus).
//!
//! ksni runs the tray on its own thread; menu actions funnel into
//! the GTK main loop through a [`glib::Sender`] carrying [`Msg`]s.
//! The menu itself is built from a [`TraySnapshot`] that the app
//! core refreshes whenever notes change.

use std::sync::mpsc::Sender as MsgSender;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Local, Utc};
use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::StandardItem;
use ksni::{MenuItem, Tray};
use uuid::Uuid;

use crate::messages::Msg;

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
}

impl PinletTray {
    /// Build a tray that sends actions over `tx` and reads menu data
    /// from `snapshot`.
    pub fn new(tx: MsgSender<Msg>, snapshot: Arc<Mutex<TraySnapshot>>) -> Self {
        Self { tx, snapshot }
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
}

impl Tray for PinletTray {
    /// Left click opens the menu instead of calling `activate`.
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        "pinlet".to_owned()
    }

    fn icon_name(&self) -> String {
        // A dedicated app icon ships with packaging (Phase 3).
        "note-edit".to_owned()
    }

    fn title(&self) -> String {
        "Pinlet".to_owned()
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.send(Msg::ToggleAll);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items = Vec::new();

        items.push(Self::item("New note", "list-add-symbolic", Msg::NewNote, &self.tx));
        items.push(Self::item("Show / hide all", "view-restore-symbolic", Msg::ToggleAll, &self.tx));

        // Dynamic section: top 5 upcoming reminders (spec §3.3).
        let snapshot = self.snapshot.lock().expect("tray snapshot poisoned").clone();
        if !snapshot.upcoming.is_empty() {
            items.push(MenuItem::Separator);
            for reminder in snapshot.upcoming {
                let label = format!(
                    "{} — {}",
                    reminder.title,
                    reminder.due.with_timezone(&Local).format("%b %e, %H:%M")
                );
                items.push(Self::item(
                    &label,
                    "alarm-symbolic",
                    Msg::FocusNote(reminder.note),
                    &self.tx,
                ));
            }
        }

        items.push(MenuItem::Separator);
        items.push(Self::item("Search…", "system-search-symbolic", Msg::Search, &self.tx));
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
