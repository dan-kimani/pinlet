//! X11 desktop windows: the mechanism that puts a window on the
//! desktop layer under compositors without the wlr-layer-shell
//! protocol — notably Ubuntu's mutter, which compiles it out.
//!
//! A window marked `_NET_WM_WINDOW_TYPE_DESKTOP` is placed by the
//! window manager on the desktop layer — above the wallpaper, below
//! every regular window, hidden from the taskbar and pager — and,
//! being a desktop window, is exempt from "show desktop" (Super+D).
//! The property and the position are applied through x11rb (a pure-Rust
//! X11 client); only the window-ID lookup needs a one-line FFI call,
//! because the safe gdk4-x11 bindings don't cover it in the 0.9 line.

use std::cell::RefCell;

use gtk4::gdk;
use gtk4::prelude::*;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ConfigureWindowAux, ConnectionExt as XProtoConnectionExt, KeyButMask, PropMode,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt;

/// The X11 (XWayland) display, opened lazily and cached.
/// `None` on sessions without XWayland. Only ever touched from the
/// GTK main thread, hence the thread-local cache.
#[allow(deprecated)]
pub fn display() -> Option<gdk::Display> {
    thread_local! {
        static DISPLAY: RefCell<Option<gdk::Display>> = const { RefCell::new(None) };
    }
    DISPLAY.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            // The backend-specific opener: gdk::Display::open uses the
            // default (Wayland) backend.
            *slot = gdk4_x11::X11Display::open(None);
        }
        slot.clone()
    })
}

/// Whether the desktop-window mechanism is available.
pub fn is_supported() -> bool {
    display().is_some()
}

/// Run `f` against the cached X11 connection, opening it lazily.
fn with_connection<T>(f: impl FnOnce(&RustConnection) -> Option<T>) -> Option<T> {
    thread_local! {
        static CONNECTION: RefCell<Option<RustConnection>> = const { RefCell::new(None) };
    }
    CONNECTION.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = x11rb::connect(None).ok().map(|(conn, _)| conn);
        }
        slot.as_ref().and_then(f)
    })
}

/// The X11 window id of a realized window's surface.
#[allow(unsafe_code)]
fn xid(window: &gtk4::Window) -> Option<u32> {
    let surface = window.surface()?;
    // Safety: a read-only getter on the surface pointer; the
    // gdk4-x11-sys version matches gdk4-x11 in the dependency graph.
    let raw = unsafe {
        gdk4_x11_sys::gdk_x11_surface_get_xid(surface.as_ptr() as *mut gdk4_x11_sys::GdkX11Surface)
    };
    (raw != 0).then_some(raw as u32)
}

/// Mark a realized window as a desktop window and position it. A desktop
/// window (`_NET_WM_WINDOW_TYPE_DESKTOP`) is placed by the window manager
/// on the desktop layer — above the wallpaper, below every regular window,
/// hidden from the taskbar and pager — and is exempt from "show desktop".
pub fn apply(window: &gtk4::Window, x: i32, y: i32) {
    let Some(id) = xid(window) else {
        return;
    };
    let atoms = with_connection(|conn| {
        let window_type = conn
            .intern_atom(false, b"_NET_WM_WINDOW_TYPE")
            .ok()?
            .reply()
            .ok()?
            .atom;
        let desktop = conn
            .intern_atom(false, b"_NET_WM_WINDOW_TYPE_DESKTOP")
            .ok()?
            .reply()
            .ok()?
            .atom;
        Some((window_type, desktop))
    });
    let Some((window_type, desktop)) = atoms else {
        return;
    };
    with_connection(|conn| {
        let _ = conn.configure_window(id, &ConfigureWindowAux::new().x(x).y(y));
        // The window type is a property (an `ATOM`-typed list holding one
        // atom), not a state request, so it is written directly rather than
        // sent as a client message. Set at realize/map so the window
        // manager applies the desktop layer from the start.
        let _ = conn.change_property32(
            PropMode::REPLACE,
            id,
            window_type,
            AtomEnum::ATOM,
            &[desktop],
        );
        let _ = conn.flush();
        Some(())
    });
}

/// Move a desktop window to `(x, y)` in root coordinates.
pub fn move_to(window: &gtk4::Window, x: i32, y: i32) {
    let Some(id) = xid(window) else {
        return;
    };
    with_connection(|conn| {
        let _ = conn.configure_window(id, &ConfigureWindowAux::new().x(x).y(y));
        let _ = conn.flush();
        Some(())
    });
}

/// The pointer position in root (screen) coordinates, plus whether button 1
/// is currently held. Used to drive a drag from a polling loop that is
/// independent of GTK's gesture lifetime: the gesture can be cancelled when
/// the pointer drifts off the moving surface, but `query_pointer` keeps
/// reporting the true position and button state regardless.
pub fn drag_pointer() -> Option<(i32, i32, bool)> {
    with_connection(|conn| {
        let root = conn.setup().roots[0].root;
        let reply = conn.query_pointer(root).ok()?.reply().ok()?;
        let mask = u16::from(reply.mask);
        let button1 = mask & u16::from(KeyButMask::BUTTON1) != 0;
        Some((i32::from(reply.root_x), i32::from(reply.root_y), button1))
    })
}
