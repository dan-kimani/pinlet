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
    /// Whether to sync the note repo with a git remote (push/pull).
    pub git_sync_enabled: bool,
    /// Remote URL for git sync (empty = unset).
    pub git_remote_url: String,
    /// Branch to push and pull.
    pub git_branch: String,
    /// Minutes between automatic local commits while syncing (0 = off).
    pub git_commit_interval_min: u64,
    /// Minutes between automatic pushes (full syncs) while syncing (0 = off).
    pub git_push_interval_min: u64,
    /// Global note text scale multiplier (per-note overrides possible).
    pub font_scale: f32,
    /// Per-tag palette color overrides, keyed by tag name. Tags
    /// without an entry use their deterministic palette color.
    pub tag_colors: std::collections::HashMap<String, String>,
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
            git_sync_enabled: false,
            git_remote_url: String::new(),
            git_branch: "main".to_owned(),
            git_commit_interval_min: 5,
            git_push_interval_min: 15,
            font_scale: 1.0,
            tag_colors: std::collections::HashMap::new(),
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
        crate::fs::atomic_write(path, raw.as_bytes())
    }
}
