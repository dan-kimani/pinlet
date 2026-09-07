//! Messages from background services (tray, notifications) into the
//! GTK main loop. Everything funnels through one glib channel
//! attached to the main context.

use chrono::{DateTime, Utc};
use uuid::Uuid;

/// Git sync status shown by the tray indicator (and preferences).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SyncState {
    /// Sync isn't set up: disabled or no remote URL configured.
    #[default]
    Unconfigured,
    /// A sync (pull → commit → push) is running right now.
    Syncing,
    /// The last sync succeeded; notes are in sync with the remote.
    InSync,
    /// A merge conflict needs manual resolution in the repository.
    Conflict,
    /// The last sync failed for a reason other than a conflict.
    Error(String),
}

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
    /// Open the preferences window.
    OpenSettings,
    /// Quit the application.
    Quit,
    /// Pull the note repo from its git remote.
    GitPull,
    /// Push the note repo to its git remote.
    GitPush,
    /// Result of the last git push/pull, shown in preferences.
    GitResult(String),
    /// Run a full sync now (pull → commit → push).
    GitSync,
    /// The sync state changed (updates the tray indicator).
    SyncState(SyncState),
}
