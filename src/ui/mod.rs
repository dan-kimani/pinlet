//! The GTK4 / libadwaita layer: note windows, colors, styling.

mod colors;
mod note_window;
pub mod password_dialog;
mod reminder_dialog;
mod settings_window;
pub mod x11;

pub use note_window::{NoteCallbacks, NoteWindow};
pub use settings_window::{SettingsCallbacks, SettingsWindow};

use std::cell::RefCell;

use gtk4::CssProvider;
use gtk4::gdk;

use note_window::STYLE;

/// Install the shared note stylesheet on the default display.
/// Every selector is scoped with a `pinlet-*` class, so the provider
/// is safe to add at application priority.
pub fn ensure_styles() {
    thread_local! {
        static PROVIDER: RefCell<Option<CssProvider>> = const { RefCell::new(None) };
    }
    PROVIDER.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            let provider = CssProvider::new();
            provider.load_from_data(STYLE);
            if let Some(display) = gdk::Display::default() {
                gtk4::style_context_add_provider_for_display(
                    &display,
                    &provider,
                    gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
                );
            }
            *slot = Some(provider);
        }
    });
}
