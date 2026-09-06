//! Desktop-pinning backend selection (spec §3.6).
//!
//! How a note pinned to the desktop is actually rendered depends on the
//! session:
//!
//! - **X11 (Xorg) / XWayland**: an X11 window marked
//!   `_NET_WM_WINDOW_TYPE_DESKTOP` sits on the desktop layer — above the
//!   wallpaper, below regular windows, and exempt from "show desktop".
//! - **Wayland + wlr-layer-shell** (KDE Plasma 6, wlroots compositors):
//!   the layer-shell protocol places the note on the background layer.

/// Which mechanism renders a note pinned to the desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinBackend {
    /// wlr-layer-shell protocol (KDE Plasma 6, wlroots compositors).
    LayerShell,
    /// X11 `_NET_WM_WINDOW_TYPE_DESKTOP` desktop window (Xorg or XWayland).
    X11,
    /// No supported mechanism; the pin action is disabled.
    None,
}

impl PinBackend {
    /// Detect the backend that works on this session.
    pub fn detect() -> Self {
        if gtk4_layer_shell::is_supported() {
            return Self::LayerShell;
        }
        // Xorg, or a Wayland session with XWayland available: a
        // `_NET_WM_WINDOW_TYPE_DESKTOP` window lands on the desktop layer
        // and survives "show desktop" (Super+D).
        if crate::ui::x11::is_supported() {
            return Self::X11;
        }
        Self::None
    }
}
