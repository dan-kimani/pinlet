//! Machine-local state (`local-state.json`): window geometry and
//! other data that must never sync (spec §4.3). The file lives in
//! the data directory but is excluded by its `.gitignore`.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};

/// File name inside the data directory (git-ignored).
pub const LOCAL_STATE_FILE: &str = "local-state.json";

/// Per-machine state. `#[serde(default)]` keeps the file tolerant of
/// hand edits and future additions.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalState {
    /// Last known window geometry per note id.
    pub window_geometry: HashMap<String, WindowGeometry>,
}

/// Window position and size. GTK4 cannot position normal windows on
/// Wayland, so `x`/`y` are X11-only there — for pinned notes they are
/// the layer-shell margins. Width and height always restore.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WindowGeometry {
    /// Screen X coordinate (X11 only).
    pub x: i32,
    /// Screen Y coordinate (X11 only).
    pub y: i32,
    /// Window width.
    pub width: i32,
    /// Window height.
    pub height: i32,
}

impl LocalState {
    /// Load from `path`; a missing file yields an empty state.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_json() {
        let path = std::env::temp_dir().join(format!("pinlet-local-state-{}.json", uuid::Uuid::new_v4()));

        let mut state = LocalState::default();
        state.window_geometry.insert(
            "abc".to_owned(),
            WindowGeometry { x: 10, y: 20, width: 300, height: 400 },
        );
        state.save(&path).unwrap();

        let loaded = LocalState::load(&path).unwrap();
        let geometry = loaded.window_geometry.get("abc").copied().unwrap();
        assert_eq!(geometry.width, 300);
        assert_eq!(geometry.height, 400);
        assert_eq!(geometry.x, 10);
        assert_eq!(geometry.y, 20);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn missing_file_loads_empty() {
        let path = std::env::temp_dir().join(format!("pinlet-missing-{}.json", uuid::Uuid::new_v4()));
        let state = LocalState::load(&path).unwrap();
        assert!(state.window_geometry.is_empty());
    }
}
