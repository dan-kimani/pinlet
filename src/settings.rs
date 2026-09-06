//! Global application settings, persisted as JSON inside the note
//! repository (`settings.json`), so preferences sync with the notes.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};

/// File name inside the data directory.
pub const SETTINGS_FILE: &str = "settings.json";

/// User-tunable application settings. `#[serde(default)]` keeps the
/// file tolerant of missing keys: hand-edited files and older
/// versions still load.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Force a single color for every note (spec Mode B).
    pub force_global_color: bool,
    /// Color used for newly created notes (a `NoteColor` string).
    pub default_color: String,
    /// Follow GNOME's system dark mode preference.
    pub sync_dark_mode: bool,
    /// Debounce delay for auto-save, in milliseconds.
    pub auto_save_debounce_ms: u64,
    /// Whether the global capture shortcut is enabled (Phase 3).
    pub enable_global_shortcut: bool,
    /// Start the app on login (Phase 3).
    pub autostart: bool,
    /// Retention for archived notes, in days (Phase 4).
    pub archive_retention_days: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            force_global_color: false,
            default_color: "Yellow".to_owned(),
            sync_dark_mode: true,
            auto_save_debounce_ms: 500,
            enable_global_shortcut: false,
            autostart: false,
            archive_retention_days: 30,
        }
    }
}

impl Settings {
    /// Load settings from `path`; a missing file yields defaults.
    pub fn load(path: &Path) -> AppResult<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path)?;
        serde_json::from_str(&raw).map_err(|err| AppError::Settings(err.to_string()))
    }

    /// Atomically persist to `path`.
    pub fn save(&self, path: &Path) -> AppResult<()> {
        let raw = serde_json::to_string_pretty(self)
            .map_err(|err| AppError::Settings(err.to_string()))?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, raw)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }
}
