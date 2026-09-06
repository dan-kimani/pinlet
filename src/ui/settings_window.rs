//! The preferences window (spec §3.5): appearance, behavior, data, sync.

use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use adw::{
    ActionRow, ComboRow, EntryRow, PreferencesGroup, PreferencesPage, PreferencesWindow, SwitchRow,
};
use gtk4::gio;
use gtk4::{CheckButton, Label, Orientation, PasswordEntry, SignalListItemFactory};

use crate::settings::Settings;
use crate::storage::NoteColor;

/// Callbacks into the app core; one per setting.
pub struct SettingsCallbacks {
    /// Force one color for all notes (spec Mode B).
    pub on_force_global_color: Box<dyn Fn(bool)>,
    /// Default color for new notes.
    pub on_default_color: Box<dyn Fn(String)>,
    /// Follow the system dark mode preference.
    pub on_sync_dark_mode: Box<dyn Fn(bool)>,
    /// Auto-save debounce in milliseconds.
    pub on_auto_save_debounce: Box<dyn Fn(u64)>,
    /// Global capture shortcut enabled.
    pub on_enable_shortcut: Box<dyn Fn(bool)>,
    /// Start on login.
    pub on_autostart: Box<dyn Fn(bool)>,
    /// Git sync enabled.
    pub on_git_sync: Box<dyn Fn(bool)>,
    /// Git remote URL changed.
    pub on_git_remote: Box<dyn Fn(String)>,
    /// Git branch changed.
    pub on_git_branch: Box<dyn Fn(String)>,
    /// Pull the note repo from its remote.
    pub on_git_pull: Box<dyn Fn()>,
    /// Push the note repo to its remote.
    pub on_git_push: Box<dyn Fn()>,
    /// Master password changed.
    pub on_master_password: Box<dyn Fn(String)>,
}

/// The six palette colors, in palette order.
pub const COLOR_NAMES: [&str; 6] = ["Yellow", "Green", "Blue", "Pink", "Purple", "Charcoal"];

/// Auto-save debounce presets, in milliseconds.
const SAVE_DELAYS_MS: [u64; 4] = [500, 1000, 2000, 5000];

/// Human-readable labels for the auto-save presets, matching [`SAVE_DELAYS_MS`].
const SAVE_DELAY_LABELS: [&str; 4] = ["0.5 s", "1 s", "2 s", "5 s"];

/// One settings window per application; shown and hidden on demand.
pub struct SettingsWindow {
    window: PreferencesWindow,
    /// Status line under the git sync controls, updated after push/pull.
    git_status: gtk4::Label,
}

impl SettingsWindow {
    /// Build the preferences window from the current settings.
    /// `shortcut_support` says whether the desktop can register a
    /// global capture shortcut; `shortcut_subtitle` explains how.
    pub fn new(
        app: &gtk4::Application,
        settings: &Settings,
        data_dir: PathBuf,
        master_password: &str,
        shortcut_support: bool,
        shortcut_subtitle: &'static str,
        callbacks: SettingsCallbacks,
    ) -> Self {
        let callbacks = Rc::new(callbacks);

        let window = PreferencesWindow::builder()
            .application(app)
            .title("Preferences")
            .default_width(480)
            .default_height(520)
            .build();
        // Scopes the preferences-only CSS rules (see style.css).
        window.add_css_class("pinlet-settings");

        // One page, grouped by concern (spec §3.5).
        let page = PreferencesPage::builder().build();
        page.set_margin_top(12);
        page.set_margin_bottom(24);
        page.set_margin_start(12);
        page.set_margin_end(12);

        let appearance_group = PreferencesGroup::builder().title("Appearance").build();

        // Note color mode: a radio pair (spec Mode A vs Mode B).
        let per_note_cb = CheckButton::builder().build();
        let uniform_cb = CheckButton::builder().build();
        uniform_cb.set_group(Some(&per_note_cb));
        per_note_cb.set_active(!settings.force_global_color);
        uniform_cb.set_active(settings.force_global_color);

        let per_note_row = ActionRow::builder()
            .title("Per-note colors")
            .subtitle("Each note keeps its own color")
            .activatable_widget(&per_note_cb)
            .build();
        pad_row(&per_note_row);
        {
            let callbacks = callbacks.clone();
            per_note_cb.connect_toggled(move |cb| {
                if cb.is_active() {
                    (callbacks.on_force_global_color)(false);
                }
            });
        }
        appearance_group.add(&per_note_row);

        let uniform_row = ActionRow::builder()
            .title("Uniform color")
            .subtitle("All notes share one color")
            .activatable_widget(&uniform_cb)
            .build();
        pad_row(&uniform_row);
        {
            let callbacks = callbacks.clone();
            uniform_cb.connect_toggled(move |cb| {
                if cb.is_active() {
                    (callbacks.on_force_global_color)(true);
                }
            });
        }
        appearance_group.add(&uniform_row);

        let color_names = gtk4::StringList::new(&COLOR_NAMES);
        let default_color_row = ComboRow::builder()
            .title("Default note color")
            .subtitle("Used for new notes and for uniform mode")
            .model(&color_names)
            .selected(selected_index(&settings.default_color))
            .build();
        // Render each entry as a color swatch + name.
        default_color_row.set_factory(Some(&color_factory()));
        pad_row(&default_color_row);
        {
            let callbacks = callbacks.clone();
            default_color_row.connect_selected_notify(move |row| {
                let name = COLOR_NAMES[row.selected() as usize].to_owned();
                (callbacks.on_default_color)(name);
            });
        }
        appearance_group.add(&default_color_row);

        let dark_row = SwitchRow::builder()
            .title("Follow system dark mode")
            .subtitle("Match the desktop's light/dark preference")
            .active(settings.sync_dark_mode)
            .build();
        pad_row(&dark_row);
        {
            let callbacks = callbacks.clone();
            dark_row.connect_active_notify(move |row| {
                (callbacks.on_sync_dark_mode)(row.is_active());
            });
        }
        appearance_group.add(&dark_row);
        page.add(&appearance_group);

        let editing_group = PreferencesGroup::builder().title("Editing").build();
        editing_group.set_margin_top(18);
        let save_row = ComboRow::builder()
            .title("Auto-save delay")
            .subtitle("Time after typing stops before a note is saved")
            .model(&gtk4::StringList::new(&SAVE_DELAY_LABELS))
            .selected(save_delay_index(settings.auto_save_debounce_ms))
            .build();
        pad_row(&save_row);
        {
            let callbacks = callbacks.clone();
            save_row.connect_selected_notify(move |row| {
                (callbacks.on_auto_save_debounce)(SAVE_DELAYS_MS[row.selected() as usize]);
            });
        }
        editing_group.add(&save_row);
        page.add(&editing_group);

        let integration_group = PreferencesGroup::builder().title("Integration").build();
        integration_group.set_margin_top(18);
        let shortcut_row = SwitchRow::builder()
            .title("Global capture shortcut")
            .subtitle(shortcut_subtitle)
            .active(settings.enable_global_shortcut)
            .build();
        shortcut_row.set_sensitive(shortcut_support);
        pad_row(&shortcut_row);
        {
            let callbacks = callbacks.clone();
            shortcut_row.connect_active_notify(move |row| {
                (callbacks.on_enable_shortcut)(row.is_active());
            });
        }
        integration_group.add(&shortcut_row);

        let autostart_row = SwitchRow::builder()
            .title("Start on login")
            .subtitle("Launch Pinlet when you sign in")
            .active(settings.autostart)
            .build();
        pad_row(&autostart_row);
        {
            let callbacks = callbacks.clone();
            autostart_row.connect_active_notify(move |row| {
                (callbacks.on_autostart)(row.is_active());
            });
        }
        integration_group.add(&autostart_row);
        page.add(&integration_group);

        let storage_group = PreferencesGroup::builder().title("Storage").build();
        storage_group.set_margin_top(18);
        let data_row = ActionRow::builder()
            .title("Data directory")
            .subtitle(data_dir.display().to_string())
            .activatable(true)
            .build();
        pad_row(&data_row);
        data_row.connect_activated(move |_| {
            let uri = format!("file://{}", data_dir.display());
            let context = gtk4::gdk::Display::default().map(|display| display.app_launch_context());
            if let Err(err) = gio::AppInfo::launch_default_for_uri(&uri, context.as_ref()) {
                eprintln!("failed to open data directory: {err}");
            }
        });
        storage_group.add(&data_row);
        page.add(&storage_group);

        let security_group = PreferencesGroup::builder().title("Security").build();
        security_group.set_margin_top(18);
        let password_row = ActionRow::builder()
            .title("Master password")
            .subtitle("Used to lock and unlock notes")
            .build();
        let password_entry = PasswordEntry::builder()
            .show_peek_icon(true)
            .hexpand(true)
            .build();
        password_entry.set_text(master_password);
        password_row.add_suffix(&password_entry);
        pad_row(&password_row);
        {
            let callbacks = callbacks.clone();
            password_entry.connect_changed(move |entry| {
                (callbacks.on_master_password)(entry.text().to_string());
            });
        }
        security_group.add(&password_row);
        page.add(&security_group);

        let git_group = PreferencesGroup::builder().title("Sync").build();
        git_group.set_margin_top(18);

        let sync_row = SwitchRow::builder()
            .title("Sync with a remote")
            .subtitle("Push and pull the note repository")
            .active(settings.git_sync_enabled)
            .build();
        pad_row(&sync_row);
        git_group.add(&sync_row);

        let remote_row = EntryRow::builder()
            .title("Remote URL")
            .text(settings.git_remote_url.as_str())
            .build();
        pad_row(&remote_row);
        git_group.add(&remote_row);

        let branch_row = EntryRow::builder()
            .title("Branch")
            .text(settings.git_branch.as_str())
            .build();
        pad_row(&branch_row);
        git_group.add(&branch_row);

        let pull_row = ActionRow::builder()
            .title("Pull now")
            .subtitle("Fetch and fast-forward from the remote")
            .activatable(true)
            .build();
        pad_row(&pull_row);
        git_group.add(&pull_row);

        let push_row = ActionRow::builder()
            .title("Push now")
            .subtitle("Push committed changes to the remote")
            .activatable(true)
            .build();
        pad_row(&push_row);
        git_group.add(&push_row);

        let git_status = Label::builder().xalign(0.0).wrap(true).build();
        git_status.add_css_class("dim-label");
        git_status.set_margin_start(16);
        git_status.set_margin_end(16);
        git_status.set_margin_top(4);
        git_group.add(&git_status);

        // The sync switch gates the remote, branch, and buttons.
        let apply_sync_state = {
            let remote_row = remote_row.clone();
            let branch_row = branch_row.clone();
            let pull_row = pull_row.clone();
            let push_row = push_row.clone();
            move |on: bool| {
                remote_row.set_sensitive(on);
                branch_row.set_sensitive(on);
                pull_row.set_sensitive(on);
                push_row.set_sensitive(on);
            }
        };
        apply_sync_state(settings.git_sync_enabled);
        {
            let callbacks = callbacks.clone();
            let apply_sync_state = apply_sync_state.clone();
            sync_row.connect_active_notify(move |row| {
                let on = row.is_active();
                apply_sync_state(on);
                (callbacks.on_git_sync)(on);
            });
        }
        {
            let callbacks = callbacks.clone();
            remote_row.connect_apply(move |row| {
                (callbacks.on_git_remote)(row.text().to_string());
            });
        }
        {
            let callbacks = callbacks.clone();
            branch_row.connect_apply(move |row| {
                (callbacks.on_git_branch)(row.text().to_string());
            });
        }
        {
            let callbacks = callbacks.clone();
            pull_row.connect_activated(move |_| {
                (callbacks.on_git_pull)();
            });
        }
        {
            let callbacks = callbacks.clone();
            push_row.connect_activated(move |_| {
                (callbacks.on_git_push)();
            });
        }

        page.add(&git_group);

        window.add(&page);

        Self {
            window,
            git_status,
        }
    }

    /// Update the git status line after a push or pull.
    pub fn set_git_status(&self, message: &str) {
        self.git_status.set_label(message);
    }

    /// Show and focus the window.
    pub fn present(&self) {
        self.window.present();
    }
}

/// Index of `default_color` in the palette, falling back to Yellow.
fn selected_index(default_color: &str) -> u32 {
    COLOR_NAMES
        .iter()
        .position(|name| *name == default_color)
        .unwrap_or(0) as u32
}

/// Index of `ms` in the auto-save presets, falling back to the first.
fn save_delay_index(ms: u64) -> u32 {
    SAVE_DELAYS_MS.iter().position(|&value| value == ms).unwrap_or(0) as u32
}

/// Horizontal padding for preference rows.
fn pad_row(row: &impl IsA<gtk4::Widget>) {
    row.set_margin_start(8);
    row.set_margin_end(8);
    row.set_margin_top(2);
    row.set_margin_bottom(2);
}

/// Factory that renders a combo entry as a color swatch plus its
/// display name.
fn color_factory() -> SignalListItemFactory {
    let factory = SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let row_box = gtk4::Box::new(Orientation::Horizontal, 8);
        let swatch = gtk4::Box::builder()
            .width_request(16)
            .height_request(16)
            .build();
        swatch.add_css_class("pinlet-swatch");
        let label = Label::new(None);
        row_box.append(&swatch);
        row_box.append(&label);
        item.set_child(Some(&row_box));
    });
    factory.connect_bind(|_, item| {
        let Some(row_box) = item.child().and_then(|w| w.downcast::<gtk4::Box>().ok()) else {
            return;
        };
        let swatch = row_box.first_child().expect("swatch");
        let label = swatch
            .next_sibling()
            .expect("label")
            .downcast::<Label>()
            .expect("label");

        let name = item
            .item()
            .and_downcast::<gtk4::StringObject>()
            .map(|object| object.string().to_string())
            .unwrap_or_default();
        let color: NoteColor = name.parse().unwrap_or(NoteColor::Yellow);
        swatch.set_css_classes(&[color.css_class()]);
        label.set_label(color.name());
    });
    factory
}
