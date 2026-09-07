//! GNOME fallback for the global capture shortcut (spec §3.8): a
//! custom keybinding in `org.gnome.settings-daemon.plugins.media-keys`.
//! Used on desktops without the GlobalShortcuts portal (GNOME < 47).
//!
//! The keybinding runs `<pinlet-binary> new`, which hands off to the
//! running instance via GtkApplication's DBus single-instance
//! activation, or starts the app when it is not running.

use gtk4::gio;
use gtk4::prelude::*;

/// Key of the active-custom-keybindings list in the media-keys
/// settings.
const KEYBINDINGS_KEY: &str = "custom-keybindings";

/// Schema of the media-keys daemon settings.
const MEDIA_KEYS_SCHEMA: &str = "org.gnome.settings-daemon.plugins.media-keys";

/// Schema of one custom keybinding entry.
const BINDING_SCHEMA: &str = "org.gnome.settings-daemon.plugins.media-keys.custom-keybinding";

/// Where our keybinding entry lives.
const BINDING_PATH: &str =
    "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/pinlet0/";

/// Combo used until the user picks their own.
const DEFAULT_BINDING: &str = "<Super>n";

/// Whether the GNOME settings-daemon schemas exist (GNOME session).
pub fn is_supported() -> bool {
    schema_exists(MEDIA_KEYS_SCHEMA) && schema_exists(BINDING_SCHEMA)
}

/// Register (or re-register) the keybinding with the settings
/// daemon. Idempotent: re-enabling keeps the user's chosen combo.
pub fn enable() {
    let Some(media) = media_keys() else {
        eprintln!("GNOME media-keys settings unavailable");
        return;
    };

    let mut list: Vec<String> = media
        .strv(KEYBINDINGS_KEY)
        .iter()
        .map(|path| path.to_string())
        .collect();
    if !list.iter().any(|path| path == BINDING_PATH) {
        list.push(BINDING_PATH.to_owned());
        let paths: Vec<&str> = list.iter().map(String::as_str).collect();
        let _ = media.set_strv(KEYBINDINGS_KEY, paths);
    }

    if let Some(binding) = binding_settings() {
        // The command always points at the current executable (it goes
        // stale across reinstalls to a new path); the key combo itself
        // is only defaulted on first creation so a re-enable keeps the
        // user's customized binding.
        let _ = binding.set_string("command", &capture_command());
        if binding.string("binding").is_empty() {
            let _ = binding.set_string("binding", DEFAULT_BINDING);
        }
        let _ = binding.set_string("name", "New Pinlet note");
    }
}

/// Remove the keybinding from the active list. The entry itself and
/// the user's combo survive for re-enable.
pub fn disable() {
    if let Some(media) = media_keys() {
        let filtered: Vec<String> = media
            .strv(KEYBINDINGS_KEY)
            .iter()
            .filter(|path| *path != BINDING_PATH)
            .map(|path| path.to_string())
            .collect();
        let paths: Vec<&str> = filtered.iter().map(String::as_str).collect();
        let _ = media.set_strv(KEYBINDINGS_KEY, paths);
    }
}

fn schema_exists(id: &str) -> bool {
    gio::SettingsSchemaSource::default()
        .and_then(|source| source.lookup(id, true))
        .is_some()
}

fn media_keys() -> Option<gio::Settings> {
    schema_exists(MEDIA_KEYS_SCHEMA).then(|| gio::Settings::new(MEDIA_KEYS_SCHEMA))
}

fn binding_settings() -> Option<gio::Settings> {
    let schema = gio::SettingsSchemaSource::default()?.lookup(BINDING_SCHEMA, true)?;
    Some(gio::Settings::new_full(
        &schema,
        None::<&gio::SettingsBackend>,
        Some(BINDING_PATH),
    ))
}

/// The command the keybinding runs: the current executable with
/// `new` (quoted in case the path contains spaces).
fn capture_command() -> String {
    std::env::current_exe().map_or_else(
        |_| "pinlet new".to_owned(),
        |path| format!("\"{}\" new", path.display()),
    )
}
