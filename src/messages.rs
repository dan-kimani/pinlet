//! Messages from background services (tray, notifications) into the
//! GTK main loop. Everything funnels through one glib channel
//! attached to the main context.

use chrono::{DateTime, Utc};
use uuid::Uuid;

/// An action requested by a background service.
#[derive(Debug, Clone)]
pub enum Msg {
    /// Open (or create) a note.
    NewNote,
    /// Focus a specific note's window.
    FocusNote(Uuid),
    /// Toggle visibility of all note windows.
    ToggleAll,
    /// Snooze the reminder with this due time by `minutes`.
    Snooze {
        /// Owning note.
        note: Uuid,
        /// The reminder's due time (identity, since it changes on snooze).
        due: DateTime<Utc>,
        /// How many minutes to push it out.
        minutes: i64,
    },
    /// Open the quick search window.
    Search,
    /// Open the preferences window.
    OpenSettings,
    /// Quit the application.
    Quit,
}
