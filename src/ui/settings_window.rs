//! The preferences window (spec §3.5): appearance, behavior, data, sync.

use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use adw::{
    ActionRow, ComboRow, EntryRow, PreferencesGroup, PreferencesPage, PreferencesWindow, SwitchRow,
};
use gtk4::gio;
use gtk4::{Button, Label, Orientation, PasswordEntry, SignalListItemFactory};

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
    /// Global note text scale changed (per-note overrides stay).
    pub on_global_font_scale: Box<dyn Fn(f32)>,
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
    /// Auto-commit interval (minutes) changed.
    pub on_git_commit_interval: Box<dyn Fn(u64)>,
    /// Auto-push interval (minutes) changed.
    pub on_git_push_interval: Box<dyn Fn(u64)>,
    /// Pull the note repo from its remote.
    pub on_git_pull: Box<dyn Fn()>,
    /// Push the note repo to its remote.
    pub on_git_push: Box<dyn Fn()>,
    /// Master password Set button pressed with the entry contents
    /// (empty clears the stored secret). Returns `Ok` on success so
    /// the UI can reflect the new state, `Err` message otherwise.
    pub on_master_password: Box<dyn Fn(String) -> Result<(), String>>,
}

/// The six palette colors, in palette order.
const COLOR_NAMES: [&str; 6] = ["Yellow", "Green", "Blue", "Pink", "Purple", "Charcoal"];

/// Auto-save debounce presets, in milliseconds.
const SAVE_DELAYS_MS: [u64; 4] = [500, 1000, 2000, 5000];

/// Global text-scale presets, as multipliers.
const FONT_SCALES: [f32; 6] = [0.8, 0.9, 1.0, 1.1, 1.25, 1.5];

/// Human-readable labels for the text-scale presets.
const FONT_SCALE_LABELS: [&str; 6] = ["80%", "90%", "100%", "110%", "125%", "150%"];

/// Human-readable labels for the auto-save presets, matching [`SAVE_DELAYS_MS`].
const SAVE_DELAY_LABELS: [&str; 4] = ["0.5 s", "1 s", "2 s", "5 s"];

/// Auto-commit / auto-push interval presets, in minutes (0 = off).
const SYNC_INTERVALS_MIN: [u64; 5] = [0, 1, 5, 15, 30];

/// Human-readable labels for the sync interval presets.
const SYNC_INTERVAL_LABELS: [&str; 5] = ["Off", "1 min", "5 min", "15 min", "30 min"];

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
        has_master_password: bool,
        shortcut_support: bool,
        shortcut_subtitle: &'static str,
        callbacks: SettingsCallbacks,
    ) -> Self {
        let callbacks = Rc::new(callbacks);

        let window = PreferencesWindow::builder()
            .application(app)
            .title("Preferences")
            .search_enabled(false)
            .default_width(480)
            .default_height(520)
            .build();

        // One page, grouped by concern (spec §3.5).
        let page = PreferencesPage::builder().build();

        let appearance_group = PreferencesGroup::builder().title("Appearance").build();

        // Note color mode: a single switch (spec Mode A vs Mode B) — off keeps
        // per-note colors, on forces one color for every note.
        let uniform_row = SwitchRow::builder()
            .title("Uniform color")
            .subtitle("All notes share one color instead of each keeping its own")
            .active(settings.force_global_color)
            .build();
        {
            let callbacks = callbacks.clone();
            uniform_row.connect_active_notify(move |row| {
                (callbacks.on_force_global_color)(row.is_active());
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
        {
            let callbacks = callbacks.clone();
            dark_row.connect_active_notify(move |row| {
                (callbacks.on_sync_dark_mode)(row.is_active());
            });
        }
        appearance_group.add(&dark_row);

        let font_row = ComboRow::builder()
            .title("Text size")
            .subtitle("Global note text size (notes can override it)")
            .model(&gtk4::StringList::new(&FONT_SCALE_LABELS))
            .selected(font_scale_index(settings.font_scale))
            .build();
        {
            let callbacks = callbacks.clone();
            font_row.connect_selected_notify(move |row| {
                (callbacks.on_global_font_scale)(FONT_SCALES[row.selected() as usize]);
            });
        }
        appearance_group.add(&font_row);
        page.add(&appearance_group);

        let editing_group = PreferencesGroup::builder().title("Editing").build();
        let save_row = ComboRow::builder()
            .title("Auto-save delay")
            .subtitle("Time after typing stops before a note is saved")
            .model(&gtk4::StringList::new(&SAVE_DELAY_LABELS))
            .selected(save_delay_index(settings.auto_save_debounce_ms))
            .build();
        {
            let callbacks = callbacks.clone();
            save_row.connect_selected_notify(move |row| {
                (callbacks.on_auto_save_debounce)(SAVE_DELAYS_MS[row.selected() as usize]);
            });
        }
        editing_group.add(&save_row);
        page.add(&editing_group);

        let integration_group = PreferencesGroup::builder().title("Integration").build();
        let shortcut_row = SwitchRow::builder()
            .title("Global capture shortcut")
            .subtitle(shortcut_subtitle)
            .active(settings.enable_global_shortcut)
            .build();
        shortcut_row.set_sensitive(shortcut_support);
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
        {
            let callbacks = callbacks.clone();
            autostart_row.connect_active_notify(move |row| {
                (callbacks.on_autostart)(row.is_active());
            });
        }
        integration_group.add(&autostart_row);
        page.add(&integration_group);

        let storage_group = PreferencesGroup::builder().title("Storage").build();
        let data_row = ActionRow::builder()
            .title("Data directory")
            .subtitle(data_dir.display().to_string())
            .activatable(true)
            .build();
        data_row.connect_activated(move |_| {
            let uri = format!("file://{}", data_dir.display());
            let context = gtk4::gdk::Display::default().map(|display| display.app_launch_context());
            if let Err(err) = gio::AppInfo::launch_default_for_uri(&uri, context.as_ref()) {
                eprintln!("failed to open data directory: {err}");
            }
        });
        storage_group.add(&data_row);
        page.add(&storage_group);

        // The secret itself lives in the system keyring, never on disk:
        // the entry only ever holds a replacement candidate, committed
        // explicitly with Set (per-keystroke writes would store
        // half-typed passwords, and prefilling would leak the secret
        // into the widget).
        let security_group = PreferencesGroup::builder().title("Security").build();
        let password_row = ActionRow::builder()
            .title("Master password")
            .subtitle(password_subtitle(has_master_password))
            .build();
        let password_entry = PasswordEntry::builder()
            .show_peek_icon(true)
            .hexpand(true)
            .placeholder_text("New master password")
            .build();
        let set_password_btn = Button::builder()
            .label("Set")
            .tooltip_text("Store this password (empty clears the stored one)")
            .build();
        let password_box = gtk4::Box::new(Orientation::Horizontal, 8);
        password_box.append(&password_entry);
        password_box.append(&set_password_btn);
        password_row.add_suffix(&password_box);
        {
            let callbacks = callbacks.clone();
            let password_row = password_row.clone();
            set_password_btn.connect_clicked(move |_| {
                let password = password_entry.text().to_string();
                // Empty means "clear the stored secret".
                let storing = !password.is_empty();
                match (callbacks.on_master_password)(password) {
                    Ok(()) => {
                        password_entry.set_text("");
                        password_row.set_subtitle(password_subtitle(storing));
                    }
                    Err(err) => {
                        password_row.set_subtitle(&format!("Could not store it: {err}"));
                    }
                }
            });
        }
        security_group.add(&password_row);
        page.add(&security_group);

        let git_group = PreferencesGroup::builder().title("Sync").build();

        let sync_row = SwitchRow::builder()
            .title("Sync with a remote")
            .subtitle("Push and pull the note repository")
            .active(settings.git_sync_enabled)
            .build();
        git_group.add(&sync_row);

        let remote_row = EntryRow::builder()
            .title("Remote URL")
            .text(settings.git_remote_url.as_str())
            .build();
        git_group.add(&remote_row);

        let branch_row = EntryRow::builder()
            .title("Branch")
            .text(settings.git_branch.as_str())
            .build();
        git_group.add(&branch_row);

        let commit_interval_row = ComboRow::builder()
            .title("Auto-commit every")
            .subtitle("Commit local changes on a timer while syncing")
            .model(&gtk4::StringList::new(&SYNC_INTERVAL_LABELS))
            .selected(sync_interval_index(settings.git_commit_interval_min))
            .build();
        git_group.add(&commit_interval_row);

        let push_interval_row = ComboRow::builder()
            .title("Push every")
            .subtitle("Pull, commit, and push on a timer while syncing")
            .model(&gtk4::StringList::new(&SYNC_INTERVAL_LABELS))
            .selected(sync_interval_index(settings.git_push_interval_min))
            .build();
        git_group.add(&push_interval_row);

        let pull_row = ActionRow::builder()
            .title("Pull now")
            .subtitle("Fetch and fast-forward from the remote")
            .activatable(true)
            .build();
        git_group.add(&pull_row);

        let push_row = ActionRow::builder()
            .title("Push now")
            .subtitle("Push committed changes to the remote")
            .activatable(true)
            .build();
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
            let commit_interval_row = commit_interval_row.clone();
            let push_interval_row = push_interval_row.clone();
            let pull_row = pull_row.clone();
            let push_row = push_row.clone();
            move |on: bool| {
                remote_row.set_sensitive(on);
                branch_row.set_sensitive(on);
                commit_interval_row.set_sensitive(on);
                push_interval_row.set_sensitive(on);
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
            commit_interval_row.connect_selected_notify(move |row| {
                (callbacks.on_git_commit_interval)(SYNC_INTERVALS_MIN[row.selected() as usize]);
            });
        }
        {
            let callbacks = callbacks.clone();
            push_interval_row.connect_selected_notify(move |row| {
                (callbacks.on_git_push_interval)(SYNC_INTERVALS_MIN[row.selected() as usize]);
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

        Self { window, git_status }
    }

    /// Update the git status line after a push or pull.
    pub fn set_git_status(&self, message: &str) {
        self.git_status.set_label(message);
    }

    /// Run `f` when the window is closed, so the app core can drop its
    /// handle — otherwise a closed Preferences window could never be
    /// reopened.
    pub fn connect_close(&self, f: impl Fn() + 'static) {
        self.window.connect_close_request(move |_| {
            f();
            gtk4::glib::Propagation::Proceed
        });
    }

    /// Show and focus the window.
    pub fn present(&self) {
        self.window.present();
    }
}

/// Status line for the master-password row, always reflecting whether
/// a secret is stored.
fn password_subtitle(set: bool) -> &'static str {
    if set {
        "Set — stored in the system keyring. Entering a new one replaces it."
    } else {
        "Not set — stored in the system keyring, never on disk."
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
    SAVE_DELAYS_MS
        .iter()
        .position(|&value| value == ms)
        .unwrap_or(0) as u32
}

/// Index of the preset nearest `scale` (hand-edited values snap to
/// the closest entry instead of mis-selecting).
fn font_scale_index(scale: f32) -> u32 {
    FONT_SCALES
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            (**a - scale)
                .abs()
                .partial_cmp(&(**b - scale).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map_or(2, |(index, _)| index) as u32
}

/// Index of `minutes` in the sync interval presets, falling back to Off.
fn sync_interval_index(minutes: u64) -> u32 {
    SYNC_INTERVALS_MIN
        .iter()
        .position(|&value| value == minutes)
        .unwrap_or(0) as u32
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
        let color = NoteColor::parse_validated(&name).unwrap_or_default();
        swatch.set_css_classes(&[color.css_class(), "pinlet-swatch"]);
        label.set_label(color.name());
    });
    factory
}
