//! Application core: note store, git repository, settings, tray,
//! reminder engine, and the open-window registry. The GTK layer
//! (see [`crate::ui`]) is driven from here.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use adw::prelude::*;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use gtk4::gio;
use gtk4::glib::{self, ControlFlow, SourceId};
use uuid::Uuid;

use crate::cli;
use crate::error::AppResult;
use crate::messages::{Msg, SyncState};
use crate::pinning::PinBackend;
use crate::settings::{SETTINGS_FILE, Settings};
use crate::storage::{
    DATA_DIR_NAME, GitRepo, LOCAL_STATE_FILE, LocalState, Note, NoteColor, NoteStore, Recurrence,
    Reminder, WindowGeometry, crypto,
};
use crate::timer::cancel_source;
use crate::tray::{PinletTray, TraySnapshot};
use crate::ui::password_dialog;
use crate::ui::{NoteCallbacks, NoteWindow, SettingsCallbacks, SettingsWindow};

/// State shared between one note window and the app core.
pub struct SharedNote {
    /// Frontmatter metadata.
    pub note: RefCell<Note>,
    /// Markdown body.
    pub body: RefCell<String>,
}

impl SharedNote {
    /// Window-friendly title: the derived title, or a fallback.
    /// Locked notes never expose their title or content.
    pub fn display_title(&self) -> String {
        if self.note.borrow().is_locked {
            return "🔒 Locked note".to_owned();
        }
        let title = Note::derive_title(&self.body.borrow());
        if title.is_empty() {
            "Untitled note".to_owned()
        } else {
            title
        }
    }
}

/// Application core. Cheap to clone; closures keep their own handle.
#[derive(Clone)]
pub struct App {
    inner: Rc<AppInner>,
}

/// Which global-shortcut mechanism is active on this desktop.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ShortcutBackend {
    /// XDG GlobalShortcuts portal (GNOME 47+).
    Portal,
    /// Custom keybinding in GNOME settings-daemon (GNOME < 47).
    GnomeSettings,
    /// No supported mechanism.
    None,
}

struct AppInner {
    gtk_app: gtk4::Application,
    /// Kept for the app's lifetime; releasing it lets GTK quit.
    #[allow(dead_code)]
    hold: gio::ApplicationHoldGuard,
    store: NoteStore,
    repo: GitRepo,
    /// Live settings; every change persists immediately.
    settings: RefCell<Settings>,
    settings_path: PathBuf,
    /// Which shortcut backend this desktop can use.
    shortcut_backend: ShortcutBackend,
    /// Gate for the portal shortcut listener: flipping the Preferences
    /// switch updates this live, so the running thread honors it without
    /// a restart.
    shortcut_gate: Arc<AtomicBool>,
    /// Whether the portal listener thread was already spawned (it can
    /// only be spawned once; the gate switches it on and off).
    shortcut_spawned: RefCell<bool>,
    /// Serializes every `git` subprocess: the sync thread and the main
    /// loop's auto-commits must never interleave and trip `index.lock`.
    git_lock: Arc<Mutex<()>>,
    /// Which mechanism renders notes pinned to the desktop.
    pin_backend: PinBackend,
    /// The single preferences window, created lazily.
    settings_window: RefCell<Option<SettingsWindow>>,
    /// Every live note, keyed by id.
    notes: RefCell<HashMap<Uuid, Rc<SharedNote>>>,
    /// Open window per note id.
    windows: RefCell<HashMap<Uuid, NoteWindow>>,
    /// Windows closing under app management (trash, reopen): their
    /// close-request handler must skip the save/commit path, which
    /// the caller already ran — otherwise the managed commit message
    /// is clobbered and an immediate commit fires.
    closing_managed: RefCell<HashSet<Uuid>>,
    /// Pending debounced save timers, one per note.
    save_sources: RefCell<HashMap<Uuid, SourceId>>,
    /// Pending coalesced auto-commit timer.
    commit_source: RefCell<Option<SourceId>>,
    /// Message for the next auto-commit; the last change wins.
    commit_message: RefCell<String>,
    /// Current git sync state, mirrored into the tray snapshot.
    sync_state: RefCell<SyncState>,
    /// Periodic auto-commit timer (sync feature).
    sync_commit_source: RefCell<Option<SourceId>>,
    /// Periodic auto-push (full sync) timer.
    sync_push_source: RefCell<Option<SourceId>>,
    /// Reminder-check tick loop.
    tick_source: RefCell<Option<SourceId>>,
    /// Debounced geometry-save timer.
    geometry_source: RefCell<Option<SourceId>>,
    /// Machine-local state (git-ignored).
    local_state: RefCell<LocalState>,
    local_state_path: PathBuf,
    /// Drain timer for the background message channel.
    msg_source: RefCell<Option<SourceId>>,
    /// Sender side of the background message channel.
    tx: std::sync::mpsc::Sender<Msg>,
    /// Tray menu snapshot, shared with the tray thread.
    tray_snapshot: Arc<Mutex<TraySnapshot>>,
    /// System tray handle (None where no tray host exists).
    /// Held purely to keep the tray thread alive; it has no Drop
    /// guard and dies with the process.
    #[allow(dead_code)]
    tray: Option<crate::tray::TrayHandle>,
}

/// Idle time before an auto-commit (spec §3.1).
const AUTO_COMMIT_IDLE: Duration = Duration::from_secs(120);

/// How often the reminder engine checks for due alarms.
const TICK_INTERVAL: Duration = Duration::from_secs(30);

/// Debounce for geometry saves (spec §3.7).
const GEOMETRY_DEBOUNCE: Duration = Duration::from_millis(500);

impl App {
    /// Load settings and state, open the note store, prepare the git
    /// repo, spawn the tray, and attach the message channel — in the
    /// standard XDG data directory.
    pub fn new(gtk_app: &gtk4::Application) -> AppResult<Self> {
        Self::new_in(gtk_app, data_dir())
    }

    /// Same as [`App::new`], with an explicit data directory
    /// (used by tests).
    pub fn new_in(gtk_app: &gtk4::Application, data_dir: PathBuf) -> AppResult<Self> {
        // A tray app must outlive its windows: without a hold, GTK
        // quits when the last window closes (which happens for a
        // moment while a pin toggle recreates its window). `hold()`
        // returns a guard that releases the hold on drop, so the guard
        // must be kept alive for the app's whole lifetime — `let _ =`
        // would drop it immediately and the hold would never stick.
        let hold = gtk_app.hold();

        let store = NoteStore::open(&data_dir)?;
        let repo = GitRepo::open_or_init(data_dir.clone())?;
        let settings_path = data_dir.join(SETTINGS_FILE);
        let mut settings = Settings::load(&settings_path)?;
        let local_state_path = data_dir.join(LOCAL_STATE_FILE);
        let local_state = LocalState::load(&local_state_path)?;

        let mut notes = HashMap::new();
        for (note, body) in store.load_all()? {
            notes.insert(
                note.id,
                Rc::new(SharedNote {
                    note: RefCell::new(note),
                    body: RefCell::new(body),
                }),
            );
        }

        // Background services (tray thread, notification threads) send
        // here; a glib timer drains the queue onto the main loop.
        let (tx, rx) = std::sync::mpsc::channel::<Msg>();

        // Global capture shortcut: pick the best backend this
        // desktop supports — the portal on GNOME 47+, the
        // settings-daemon keybinding on older GNOME.
        let shortcut_backend = if crate::shortcuts::is_supported() {
            ShortcutBackend::Portal
        } else if crate::gnome_shortcut::is_supported() {
            ShortcutBackend::GnomeSettings
        } else {
            ShortcutBackend::None
        };
        let shortcut_gate = Arc::new(AtomicBool::new(settings.enable_global_shortcut));
        let mut shortcut_spawned = false;
        if settings.enable_global_shortcut {
            match shortcut_backend {
                ShortcutBackend::Portal => {
                    crate::shortcuts::spawn(tx.clone(), shortcut_gate.clone());
                    shortcut_spawned = true;
                }
                ShortcutBackend::GnomeSettings => crate::gnome_shortcut::enable(),
                // No supported mechanism: self-heal settings migrated
                // from a supporting system.
                ShortcutBackend::None => {
                    settings.enable_global_shortcut = false;
                    shortcut_gate.store(false, Ordering::SeqCst);
                    let _ = settings.save(&settings_path);
                }
            }
        }

        // System tray (best effort — some environments have no host).
        let tray_snapshot = Arc::new(Mutex::new(TraySnapshot::default()));
        let tray = match crate::tray::spawn(PinletTray::new(tx.clone(), tray_snapshot.clone())) {
            Ok(handle) => Some(handle),
            Err(err) => {
                eprintln!("system tray unavailable: {err}");
                None
            }
        };

        let inner = Rc::new(AppInner {
            gtk_app: gtk_app.clone(),
            hold,
            store,
            repo,
            settings: RefCell::new(settings),
            settings_path,
            shortcut_backend,
            shortcut_gate,
            shortcut_spawned: RefCell::new(shortcut_spawned),
            git_lock: Arc::new(Mutex::new(())),
            pin_backend: PinBackend::detect(),
            settings_window: RefCell::new(None),
            notes: RefCell::new(notes),
            windows: RefCell::new(HashMap::new()),
            closing_managed: RefCell::new(HashSet::new()),
            save_sources: RefCell::new(HashMap::new()),
            commit_source: RefCell::new(None),
            commit_message: RefCell::new(String::new()),
            sync_state: RefCell::new(SyncState::default()),
            sync_commit_source: RefCell::new(None),
            sync_push_source: RefCell::new(None),
            tick_source: RefCell::new(None),
            geometry_source: RefCell::new(None),
            local_state: RefCell::new(local_state),
            local_state_path,
            msg_source: RefCell::new(None),
            tx,
            tray_snapshot,
            tray,
        });

        let app = Self { inner };
        let msg_source = {
            let this = app.clone();
            glib::timeout_add_local(Duration::from_millis(200), move || {
                while let Ok(msg) = rx.try_recv() {
                    this.handle_msg(msg);
                }
                ControlFlow::Continue
            })
        };
        *app.inner.msg_source.borrow_mut() = Some(msg_source);

        Ok(app)
    }

    /// Open every note's window, honor the CLI quick-capture command,
    /// install accelerators, and start the reminder tick loop.
    pub fn activate(&self, cli: &cli::Cli) {
        let ids: Vec<Uuid> = self
            .inner
            .notes
            .borrow()
            .values()
            .filter(|shared| !shared.note.borrow().is_trashed)
            .map(|shared| shared.note.borrow().id)
            .collect();
        for id in ids {
            self.open_window(id);
        }

        match &cli.command {
            Some(cli::Command::New { text, color }) => {
                let color = color
                    .as_deref()
                    .and_then(NoteColor::parse_validated)
                    .unwrap_or_else(|| self.default_color());
                self.new_note(color, text.clone().unwrap_or_default());
            }
            Some(cli::Command::Sync) => self.sync_now(),
            _ if self.inner.notes.borrow().is_empty() => {
                self.new_note(self.default_color(), String::new());
            }
            _ => {}
        }

        self.install_actions();

        // Catch up on reminders missed while the app was down, then
        // keep checking on a tick.
        self.check_due_reminders();
        let this = self.clone();
        self.inner
            .tick_source
            .replace(Some(glib::timeout_add_local(TICK_INTERVAL, move || {
                this.check_due_reminders();
                ControlFlow::Continue
            })));
        self.refresh_tray_snapshot();

        // Establish the sync indicator and start the timed commit/push
        // loops. A configured remote pulls on launch to reflect the
        // true sync state from the start.
        self.reschedule_sync_timers();
        let configured = self.sync_configured();
        if configured {
            self.sync_now();
        } else {
            self.set_sync_state(SyncState::Unconfigured);
        }
    }

    /// Handle command-line arguments arriving at a running instance
    /// (GtkApplication remote activation / single-instance handoff).
    pub fn handle_command_line(&self, cli: &cli::Cli) {
        match &cli.command {
            Some(cli::Command::New { text, color }) => {
                let color = color
                    .as_deref()
                    .and_then(NoteColor::parse_validated)
                    .unwrap_or_else(|| self.default_color());
                self.new_note(color, text.clone().unwrap_or_default());
            }
            Some(cli::Command::Sync) => self.sync_now(),
            // `where` over remote activation: answer like the fresh
            // process does instead of silently presenting windows.
            Some(cli::Command::Where) => println!("{}", data_dir().display()),
            _ => {
                // Plain re-invocation: bring the notes forward.
                for window in self.inner.windows.borrow().values() {
                    window.present();
                }
            }
        }
    }

    /// The default color for new notes, from settings. Hand-edited
    /// garbage falls back to Yellow instead of becoming a broken
    /// custom color.
    fn default_color(&self) -> NoteColor {
        let settings = self.inner.settings.borrow();
        NoteColor::parse_validated(&settings.default_color).unwrap_or(NoteColor::Yellow)
    }

    /// Register application actions with keyboard accelerators.
    fn install_actions(&self) {
        let app = &self.inner.gtk_app;

        {
            let this = self.clone();
            let action = gio::SimpleAction::new("new-note", None);
            action.connect_activate(move |_, _| {
                this.new_note(this.default_color(), String::new());
            });
            app.add_action(&action);
            app.set_accels_for_action("app.new-note", &["<Primary>n"]);
        }
        {
            let this = self.clone();
            let action = gio::SimpleAction::new("preferences", None);
            action.connect_activate(move |_, _| this.open_settings());
            app.add_action(&action);
            app.set_accels_for_action("app.preferences", &["<Primary>comma"]);
        }
        {
            let this = self.clone();
            let action = gio::SimpleAction::new("quit", None);
            action.connect_activate(move |_, _| {
                this.flush_all();
                this.inner.gtk_app.quit();
            });
            app.add_action(&action);
        }
        // Ctrl+Q / Ctrl+W close the focused window (via GTK's built-in
        // `win.close` action) rather than quitting the whole app. Quit
        // stays reachable through the tray menu. This matters most for
        // the preferences window, where Ctrl+Q must not tear down the
        // tray along with the window.
        app.set_accels_for_action("win.close", &["<Primary>q", "<Primary>w"]);
    }

    /// Handle a message from the tray or a notification action.
    fn handle_msg(&self, msg: Msg) {
        match msg {
            Msg::NewNote => {
                self.new_note(self.default_color(), String::new());
            }
            Msg::FocusNote(id) => self.open_note(id),
            Msg::ToggleAll => self.toggle_all(),
            Msg::Snooze { note, due, minutes } => self.snooze_reminder(note, due, minutes),
            Msg::MarkDone { note, due } => self.resolve_reminder(note, due),
            Msg::RestoreNote(id) => self.restore_note(id),
            Msg::DeleteForever(id) => self.delete_note(id),
            Msg::EmptyTrash => self.empty_trash(),
            Msg::OpenSettings => self.open_settings(),
            Msg::GitPull => self.git_pull(),
            Msg::GitPush => self.git_push(),
            Msg::GitResult(message) => self.set_git_status(&message),
            Msg::GitSync => self.sync_now(),
            Msg::SyncState(state) => self.set_sync_state(state),
            Msg::Quit => {
                self.flush_all();
                self.inner.gtk_app.quit();
            }
        }
    }

    /// Pull the note repo from its configured remote, off the main loop.
    fn git_pull(&self) {
        let repo = self.inner.repo.clone();
        let branch = self.inner.settings.borrow().git_branch.clone();
        let url = self.inner.settings.borrow().git_remote_url.clone();
        let tx = self.inner.tx.clone();
        let git_lock = self.inner.git_lock.clone();
        std::thread::spawn(move || {
            let _guard = git_lock.lock().expect("git lock poisoned");
            let result = repo
                .configure_sync(&url, &branch)
                .and_then(|_| repo.pull(&branch));
            let message = match result {
                Ok(()) => "Pulled".to_owned(),
                Err(err) => format!("Pull failed: {err}"),
            };
            let _ = tx.send(Msg::GitResult(message));
        });
    }

    /// Push the note repo to its configured remote, off the main loop.
    fn git_push(&self) {
        let repo = self.inner.repo.clone();
        let branch = self.inner.settings.borrow().git_branch.clone();
        let url = self.inner.settings.borrow().git_remote_url.clone();
        let tx = self.inner.tx.clone();
        let git_lock = self.inner.git_lock.clone();
        std::thread::spawn(move || {
            let _guard = git_lock.lock().expect("git lock poisoned");
            let result = repo
                .configure_sync(&url, &branch)
                .and_then(|_| repo.push(&branch));
            let message = match result {
                Ok(()) => "Pushed".to_owned(),
                Err(err) => format!("Push failed: {err}"),
            };
            let _ = tx.send(Msg::GitResult(message));
        });
    }

    /// Show the result of a git push/pull in the preferences window.
    fn set_git_status(&self, message: &str) {
        if let Some(window) = self.inner.settings_window.borrow().as_ref() {
            window.set_git_status(message);
        }
    }

    /// Whether sync is fully configured (enabled and a remote URL set).
    fn sync_configured(&self) -> bool {
        let settings = self.inner.settings.borrow();
        settings.git_sync_enabled && !settings.git_remote_url.trim().is_empty()
    }

    /// Run a full sync: pull, commit, then push. No-op while a sync is
    /// already in flight, and it warns instead when sync isn't set up.
    fn sync_now(&self) {
        if !self.sync_configured() {
            self.set_sync_state(SyncState::Unconfigured);
            self.notify_sync_warning(
                "Sync is not set up yet. Enable it and set a Remote URL in Preferences.",
            );
            return;
        }
        if *self.inner.sync_state.borrow() == SyncState::Syncing {
            return;
        }
        self.set_sync_state(SyncState::Syncing);

        let repo = self.inner.repo.clone();
        let (url, branch) = {
            let settings = self.inner.settings.borrow();
            (
                settings.git_remote_url.trim().to_owned(),
                settings.git_branch.clone(),
            )
        };
        let commit_message = std::mem::take(&mut *self.inner.commit_message.borrow_mut());
        let tx = self.inner.tx.clone();
        let git_lock = self.inner.git_lock.clone();
        std::thread::spawn(move || {
            let _guard = git_lock.lock().expect("git lock poisoned");
            let outcome = run_sync(&repo, &url, &branch, &commit_message);
            let _ = tx.send(Msg::SyncState(outcome));
        });
    }

    /// Commit any pending local changes (periodic auto-commit).
    /// Serialized against the sync thread like [`App::commit_now`].
    /// Skipped while a merge conflict awaits manual resolution:
    /// committing then only fails noisily and would flap the tray's
    /// Conflict indicator into a generic error.
    fn commit_auto(&self) {
        if !self.inner.settings.borrow().git_sync_enabled {
            return;
        }
        if self.inner.repo.has_conflicts().unwrap_or(false) {
            return;
        }
        let _guard = self.inner.git_lock.lock().expect("git lock poisoned");
        let pending = std::mem::take(&mut *self.inner.commit_message.borrow_mut());
        let message = if pending.is_empty() {
            "Auto-commit".to_owned()
        } else {
            pending
        };
        if let Err(err) = self.inner.repo.commit_all(&message) {
            eprintln!("auto-commit failed: {err}");
        }
    }

    /// (Re)start the periodic commit and push timers to match the
    /// current sync settings. Cancels both when sync is off.
    fn reschedule_sync_timers(&self) {
        if let Some(source) = self.inner.sync_commit_source.borrow_mut().take() {
            cancel_source(source);
        }
        if let Some(source) = self.inner.sync_push_source.borrow_mut().take() {
            cancel_source(source);
        }
        if !self.sync_configured() {
            return;
        }

        let (commit_min, push_min) = {
            let settings = self.inner.settings.borrow();
            (
                settings.git_commit_interval_min,
                settings.git_push_interval_min,
            )
        };

        if commit_min > 0 {
            let this = self.clone();
            let source = glib::timeout_add_local(Duration::from_secs(commit_min * 60), move || {
                this.commit_auto();
                ControlFlow::Continue
            });
            *self.inner.sync_commit_source.borrow_mut() = Some(source);
        }
        if push_min > 0 {
            let this = self.clone();
            let source = glib::timeout_add_local(Duration::from_secs(push_min * 60), move || {
                this.sync_now();
                ControlFlow::Continue
            });
            *self.inner.sync_push_source.borrow_mut() = Some(source);
        }
    }

    /// Record a sync-state change, mirroring it to the tray and the
    /// preferences status line.
    fn set_sync_state(&self, state: SyncState) {
        *self.inner.sync_state.borrow_mut() = state.clone();
        if let Ok(mut snapshot) = self.inner.tray_snapshot.lock() {
            snapshot.sync = state.clone();
        }
        self.set_git_status(&sync_state_text(&state));
    }

    /// React to a sync-related setting change: reschedule timers and
    /// re-derive the indicator.
    fn sync_settings_changed(&self) {
        self.reschedule_sync_timers();
        if !self.sync_configured() {
            self.set_sync_state(SyncState::Unconfigured);
        } else if *self.inner.sync_state.borrow() == SyncState::Unconfigured {
            // Just became configured — run an initial sync.
            self.sync_now();
        }
    }

    /// Surface a sync warning as a desktop notification.
    fn notify_sync_warning(&self, message: &str) {
        if let Err(err) = notify_rust::Notification::new()
            .appname("Pinlet")
            .summary("Pinlet sync")
            .body(message)
            .timeout(notify_rust::Timeout::Milliseconds(8_000))
            .show()
        {
            eprintln!("notification failed: {err}");
        }
    }

    /// Create a note (window included) and persist it immediately.
    fn new_note(&self, color: NoteColor, text: String) -> Uuid {
        let id = Uuid::new_v4();
        let shared = Rc::new(SharedNote {
            note: RefCell::new(Note::new(id, color)),
            body: RefCell::new(text),
        });
        self.inner.notes.borrow_mut().insert(id, shared.clone());
        self.open_window(id);
        self.save_now(id);
        let title = shared.display_title();
        *self.inner.commit_message.borrow_mut() = format!("Create note '{title}'");
        self.schedule_commit();
        id
    }

    /// Open a window for note `id`, wiring its callbacks. Trashed
    /// notes never open windows; restore them first.
    fn open_window(&self, id: Uuid) {
        let shared = self
            .inner
            .notes
            .borrow()
            .get(&id)
            .filter(|shared| !shared.note.borrow().is_trashed)
            .expect("note exists and is not trashed")
            .clone();

        // A partial geometry entry (missing keys default to zero)
        // falls back to the default size instead of a zero window.
        let geometry = self
            .inner
            .local_state
            .borrow()
            .window_geometry
            .get(&id.to_string())
            .copied();

        let callbacks = NoteCallbacks {
            on_changed: Box::new({
                let this = self.clone();
                move |content| this.debounced_save(id, content)
            }),
            on_delete: Box::new({
                let this = self.clone();
                move || this.trash_note(id)
            }),
            on_new: Box::new({
                let this = self.clone();
                move || {
                    this.new_note(this.default_color(), String::new());
                }
            }),
            on_close: Box::new({
                let this = self.clone();
                move || {
                    this.save_now(id);
                    this.save_geometry();
                    // The window is being destroyed: drop the registry
                    // entry so the note can be reopened later, and dead
                    // windows don't poison geometry saves or toggle-all.
                    this.inner.windows.borrow_mut().remove(&id);
                    this.commit_now();
                }
            }),
            on_add_reminder: Box::new({
                let this = self.clone();
                move |due, recurrence| this.add_reminder(id, due, recurrence)
            }),
            on_delete_reminder: Box::new({
                let this = self.clone();
                move |index| this.delete_reminder(id, index)
            }),
            on_geometry_changed: Box::new({
                let this = self.clone();
                move || this.geometry_changed()
            }),
            on_toggle_pin: Box::new({
                let this = self.clone();
                move || this.toggle_pin(id)
            }),
            on_lock_requested: Box::new({
                let this = self.clone();
                move || this.lock_or_unlock(id)
            }),
            on_tags_changed: Box::new({
                let this = self.clone();
                move |tags| this.update_tags(id, tags)
            }),
            on_font_scale: Box::new({
                let this = self.clone();
                move |scale| this.set_note_font_scale(id, scale)
            }),
        };

        let window = NoteWindow::new(
            &self.inner.gtk_app,
            shared.clone(),
            geometry,
            callbacks,
            self.inner.pin_backend,
            self.inner.settings.borrow().tag_colors.clone(),
            crate::ui::clamp_font_scale(self.inner.settings.borrow().font_scale),
        );
        self.inner.windows.borrow_mut().insert(id, window.clone());
        window.present();
        // Mode B (uniform colors) overrides the note's own color.
        self.apply_color_for(id, &window);
    }

    /// Lock an unlocked note, or unlock a locked one. Locking uses the
    /// master password set in Preferences (no prompt); unlocking still
    /// prompts, so the note stays unreadable without the password.
    fn lock_or_unlock(&self, id: Uuid) {
        let Some(window) = self.inner.windows.borrow().get(&id).cloned() else {
            return;
        };
        let is_locked = self
            .inner
            .notes
            .borrow()
            .get(&id)
            .is_some_and(|shared| shared.note.borrow().is_locked);

        let this = self.clone();
        let dialog_window = window.window().clone();
        if is_locked {
            password_dialog::present(&dialog_window, move |password| {
                this.unlock_note(id, &password);
            });
        } else {
            match crate::secret::get_master_password() {
                Ok(Some(password)) if !password.is_empty() => self.lock_note(id, &password),
                Ok(_) => self.show_no_password_alert(&dialog_window),
                Err(err) => {
                    if let Some(window) = self.inner.windows.borrow().get(&id) {
                        window.show_error(&format!("Could not read the master password: {err}"));
                    }
                }
            }
        }
    }

    /// Tell the user to set a master password in Preferences first.
    fn show_no_password_alert(&self, parent: &gtk4::Window) {
        let dialog = adw::MessageDialog::builder()
            .heading("No password set")
            .body("Set a master password in Preferences before locking notes.")
            .build();
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("prefs", "Open Preferences");
        dialog.set_response_appearance("prefs", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("prefs"));
        dialog.set_transient_for(Some(parent));
        let this = self.clone();
        dialog.connect_response(None, move |dialog, response| {
            if response == "prefs" {
                this.open_settings();
            }
            dialog.close();
        });
        dialog.present();
    }

    /// Encrypt the note and move it to the local-only `locked/` dir.
    fn lock_note(&self, id: Uuid, password: &str) {
        let Some(shared) = self.inner.notes.borrow().get(&id).cloned() else {
            return;
        };
        let body = shared.body.borrow().clone();
        let mut note = shared.note.borrow_mut();
        let title = Note::derive_title(&body);
        let plaintext = format!("{title}\n{body}");

        let blob = match crypto::encrypt(password, &plaintext) {
            Ok(blob) => blob,
            Err(err) => {
                eprintln!("failed to encrypt note {id}: {err}");
                return;
            }
        };

        // Roll back the in-memory flags when the write fails, so a
        // half-locked note (memory says locked, disk says plain) can
        // never escape this function.
        let was_locked = note.is_locked;
        let old_title = note.title.clone();
        note.is_locked = true;
        note.title = "Locked".to_owned();
        if let Err(err) = self.inner.store.save(&mut note, &blob) {
            note.is_locked = was_locked;
            note.title = old_title;
            eprintln!("failed to save locked note {id}: {err}");
            return;
        }
        drop(note);
        // The in-memory body becomes the blob: unlocking decrypts it,
        // and later saves rewrite the same ciphertext (never plaintext).
        *shared.body.borrow_mut() = blob;
        // The plaintext copy in notes/ must go.
        let plain_path = self.inner.store.path_for(id);
        if plain_path.exists()
            && let Err(err) = std::fs::remove_file(plain_path)
        {
            eprintln!("failed to remove plaintext note {id}: {err}");
        }
        *self.inner.commit_message.borrow_mut() = format!("Lock note '{title}'");
        self.schedule_commit();
        self.refresh_tray_snapshot();
        self.reopen_window(id);
    }

    /// Decrypt a locked note and move it back to the synced repo.
    fn unlock_note(&self, id: Uuid, password: &str) {
        let Some(shared) = self.inner.notes.borrow().get(&id).cloned() else {
            return;
        };
        let blob = shared.body.borrow().clone();

        let plaintext = match crypto::decrypt(password, &blob) {
            Ok(plaintext) => plaintext,
            Err(err) => {
                if let Some(window) = self.inner.windows.borrow().get(&id) {
                    window.show_error(&err.to_string());
                }
                return;
            }
        };

        let (title, body) = plaintext
            .split_once('\n')
            .map_or((plaintext.as_str(), ""), |(title, body)| (title, body));
        let mut note = shared.note.borrow_mut();
        let was_locked = note.is_locked;
        let old_title = note.title.clone();
        note.is_locked = false;
        note.title = title.to_owned();
        if let Err(err) = self.inner.store.save(&mut note, body) {
            note.is_locked = was_locked;
            note.title = old_title;
            eprintln!("failed to save unlocked note {id}: {err}");
            return;
        }
        drop(note);
        *shared.body.borrow_mut() = body.to_owned();

        let locked_path = self.inner.store.locked_dir().join(format!("{id}.md"));
        if locked_path.exists()
            && let Err(err) = std::fs::remove_file(&locked_path)
        {
            eprintln!("failed to remove locked file for note {id}: {err}");
        }
        let display = if title.is_empty() {
            "Untitled note"
        } else {
            title
        };
        *self.inner.commit_message.borrow_mut() = format!("Unlock note '{display}'");
        self.schedule_commit();
        self.refresh_tray_snapshot();
        self.reopen_window(id);
    }

    /// Close and reopen a note's window so it reflects new state.
    fn reopen_window(&self, id: Uuid) {
        // Persist geometry first: the window is dropped from the
        // registry below, so the close-request handler's save would
        // otherwise miss a drag that hasn't debounced yet.
        self.save_geometry();
        // Drop the registry borrow before closing: the close-request
        // handler runs synchronously and borrows the registries.
        let window = self.inner.windows.borrow_mut().remove(&id);
        if let Some(window) = window {
            window.close();
        }
        self.open_window(id);
    }

    /// Toggle a note between a normal window and a desktop-pinned
    /// layer-shell window (Wayland only, spec §3.6). The window is
    /// recreated so the layer is applied from the start.
    fn toggle_pin(&self, id: Uuid) {
        if let Some(shared) = self.inner.notes.borrow().get(&id) {
            let mut note = shared.note.borrow_mut();
            note.is_pinned_to_desktop = !note.is_pinned_to_desktop;
        }
        self.save_now(id);
        self.reopen_window(id);
    }

    /// Bring a note's window to the foreground. A note pinned to the
    /// desktop lives below regular windows, so it is unpinned first to let
    /// the window come to the front.
    fn open_note(&self, id: Uuid) {
        if !self
            .inner
            .notes
            .borrow()
            .get(&id)
            .is_some_and(|shared| !shared.note.borrow().is_trashed)
        {
            return;
        }
        if self
            .inner
            .notes
            .borrow()
            .get(&id)
            .is_some_and(|shared| shared.note.borrow().is_pinned_to_desktop)
        {
            self.toggle_pin(id);
        } else if self.inner.windows.borrow().contains_key(&id) {
            self.focus_note(id);
        } else {
            self.open_window(id);
        }
    }

    /// Bring a note's window to the foreground.
    fn focus_note(&self, id: Uuid) {
        if let Some(window) = self.inner.windows.borrow().get(&id) {
            window.present();
        }
    }

    /// Tray action: hide everything if anything is visible,
    /// otherwise show everything.
    fn toggle_all(&self) {
        let (any_visible, windows): (bool, Vec<NoteWindow>) = {
            let windows = self.inner.windows.borrow();
            (
                windows.values().any(NoteWindow::is_visible),
                windows.values().cloned().collect(),
            )
        };
        for window in &windows {
            if any_visible {
                window.set_visible(false);
            } else {
                window.present();
            }
        }
    }

    /// Move the note to the trash instead of deleting it: flagged,
    /// window closed, hidden everywhere until restored or emptied.
    fn trash_note(&self, id: Uuid) {
        let Some(shared) = self.inner.notes.borrow().get(&id).cloned() else {
            return;
        };
        let title = shared.display_title();
        {
            let mut note = shared.note.borrow_mut();
            note.is_trashed = true;
            note.trashed_at = Some(Utc::now());
        }
        let window = self.inner.windows.borrow_mut().remove(&id);
        if let Some(source) = self.inner.save_sources.borrow_mut().remove(&id) {
            cancel_source(source);
        }
        // Persist through the normal path so the title, timestamps,
        // and tray all stay consistent.
        self.save_now(id);
        *self.inner.commit_message.borrow_mut() = format!("Trash note '{title}'");
        self.schedule_commit();
        self.save_geometry();
        // Close last: the close-request handler runs synchronously
        // and touches the registries — no borrow may be held here.
        // Managed close: the Trash commit above must survive.
        if let Some(window) = window {
            self.inner.closing_managed.borrow_mut().insert(id);
            window.close();
            self.inner.closing_managed.borrow_mut().remove(&id);
        }
    }

    /// Restore a trashed note and reopen its window.
    fn restore_note(&self, id: Uuid) {
        let Some(shared) = self.inner.notes.borrow().get(&id).cloned() else {
            return;
        };
        if !shared.note.borrow().is_trashed {
            // Already out of the trash: just bring it forward.
            self.open_note(id);
            return;
        }
        {
            let mut note = shared.note.borrow_mut();
            note.is_trashed = false;
            note.trashed_at = None;
        }
        let title = shared.display_title();
        self.save_now(id);
        *self.inner.commit_message.borrow_mut() = format!("Restore note '{title}'");
        self.schedule_commit();
        self.open_window(id);
    }

    /// Permanently delete every trashed note. Git history still holds
    /// their content, like any other deletion.
    fn empty_trash(&self) {
        let ids: Vec<Uuid> = self
            .inner
            .notes
            .borrow()
            .values()
            .filter(|shared| shared.note.borrow().is_trashed)
            .map(|shared| shared.note.borrow().id)
            .collect();
        for id in ids {
            self.delete_note(id);
        }
    }

    /// Replace the note's tags (normalized, deduplicated, capped)
    /// and persist.
    fn update_tags(&self, id: Uuid, tags: Vec<String>) {
        let clean = Note::clean_tags(tags);
        if let Some(shared) = self.inner.notes.borrow().get(&id) {
            shared.note.borrow_mut().tags = clean;
        }
        self.save_now(id);
    }

    /// Store the per-note text scale (`None` = follow global),
    /// persist, and restyle the window.
    fn set_note_font_scale(&self, id: Uuid, scale: Option<f32>) {
        let scale = scale.map(crate::ui::clamp_font_scale);
        if let Some(shared) = self.inner.notes.borrow().get(&id) {
            shared.note.borrow_mut().font_scale = scale;
        }
        self.save_now(id);
        if let Some(window) = self.inner.windows.borrow().get(&id) {
            window.apply_font_scale(window.effective_scale());
        }
    }

    /// Re-derive every window's text scale from the global setting.
    /// Per-note overrides stay untouched.
    fn apply_font_mode(&self) {
        let base = crate::ui::clamp_font_scale(self.inner.settings.borrow().font_scale);
        for window in self.inner.windows.borrow().values() {
            window.set_base_scale(base);
        }
    }

    /// Remove the note from disk and from the registry.
    fn delete_note(&self, id: Uuid) {
        let Some(shared) = self.inner.notes.borrow_mut().remove(&id) else {
            return;
        };
        let title = shared.display_title();
        let window = self.inner.windows.borrow_mut().remove(&id);
        self.inner
            .local_state
            .borrow_mut()
            .window_geometry
            .remove(&id.to_string());
        if let Err(err) = self.inner.store.delete(id) {
            eprintln!("failed to delete note {id}: {err}");
        }
        if let Some(source) = self.inner.save_sources.borrow_mut().remove(&id) {
            cancel_source(source);
        }
        *self.inner.commit_message.borrow_mut() = format!("Delete note '{title}'");
        self.schedule_commit();
        self.refresh_tray_snapshot();
        // Close last: the close-request handler runs synchronously
        // and touches the registries — no borrow may be held here.
        if let Some(window) = window {
            window.close();
        }
    }

    /// Replace the note body and (re)start the debounced save timer.
    /// Locked notes show a placeholder instead of the body: content
    /// arriving here (a palette pick, a programmatic change) is not the
    /// note and must never overwrite the encrypted blob.
    fn debounced_save(&self, id: Uuid, content: String) {
        if let Some(shared) = self.inner.notes.borrow().get(&id) {
            if !shared.note.borrow().is_locked {
                *shared.body.borrow_mut() = content;
            }
        }
        if let Some(source) = self.inner.save_sources.borrow_mut().remove(&id) {
            cancel_source(source);
        }
        let delay = Duration::from_millis(self.inner.settings.borrow().auto_save_debounce_ms);
        let this = self.clone();
        let source = glib::timeout_add_local_once(delay, move || this.save_now(id));
        self.inner.save_sources.borrow_mut().insert(id, source);
    }

    /// Persist the note, refresh the index and tray, and schedule an
    /// auto-commit.
    fn save_now(&self, id: Uuid) {
        let Some(shared) = self.persist_note(id) else {
            return;
        };
        let display = shared.display_title();
        *self.inner.commit_message.borrow_mut() = format!("Update '{display}'");
        self.schedule_commit();
        self.refresh_tray_snapshot();
    }

    /// Write one note's file (deriving its title) without scheduling
    /// commits or refreshing the tray — batch callers do that once.
    /// Returns the note on success, `None` when it's gone or the write
    /// failed.
    fn persist_note(&self, id: Uuid) -> Option<Rc<SharedNote>> {
        let shared = self.inner.notes.borrow().get(&id).cloned()?;
        let body = shared.body.borrow().clone();
        {
            let mut note = shared.note.borrow_mut();
            // For locked notes `body` is the encrypted blob: never derive
            // a title from it.
            if !note.is_locked {
                note.title = Note::derive_title(&body);
            }
            if let Err(err) = self.inner.store.save(&mut note, &body) {
                eprintln!("failed to save note {id}: {err}");
                return None;
            }
        }
        Some(shared)
    }

    /// (Re)start the coalesced auto-commit timer.
    fn schedule_commit(&self) {
        if let Some(source) = self.inner.commit_source.borrow_mut().take() {
            cancel_source(source);
        }
        let this = self.clone();
        let source = glib::timeout_add_local_once(AUTO_COMMIT_IDLE, move || this.commit_now());
        *self.inner.commit_source.borrow_mut() = Some(source);
    }

    /// Commit pending changes, if any. Serialized against the sync
    /// thread: two concurrent `git` processes trip over `index.lock`.
    /// Like [`App::commit_auto`], skipped mid-conflict.
    fn commit_now(&self) {
        if self.inner.repo.has_conflicts().unwrap_or(false) {
            return;
        }
        let _guard = self.inner.git_lock.lock().expect("git lock poisoned");
        if let Some(source) = self.inner.commit_source.borrow_mut().take() {
            cancel_source(source);
        }
        let message = self.inner.commit_message.borrow().clone();
        if message.is_empty() {
            return;
        }
        match self.inner.repo.commit_all(&message) {
            Ok(()) => *self.inner.commit_message.borrow_mut() = String::new(),
            Err(err) => eprintln!("auto-commit failed: {err}"),
        }
    }

    /// Fire every due reminder; runs at startup (catch-up, spec
    /// §3.4) and on each tick.
    fn check_due_reminders(&self) {
        let now = Utc::now();
        let mut fired = Vec::new();
        let mut changed = Vec::new();

        for (id, shared) in self.inner.notes.borrow().iter() {
            let mut note = shared.note.borrow_mut();
            if note.is_trashed {
                // Trashed notes stay silent until restored.
                continue;
            }
            let title = note.title.clone();
            let mut dirty = false;
            for reminder in &mut note.reminders {
                let already = reminder
                    .last_fired_at
                    .is_some_and(|last| last >= reminder.due_at);
                if reminder.due_at <= now && !already {
                    // Recurring reminders reschedule instead of going
                    // quiet: advance past the present first, so the fired
                    // record (and any later snooze of it) refers to the
                    // upcoming occurrence rather than a due time that no
                    // longer exists. Missed occurrences collapse into this
                    // single firing rather than backlogging.
                    if reminder.recurrence_rule != Recurrence::None {
                        let mut due = reminder.due_at;
                        for _ in 0..366 {
                            due = reminder.recurrence_rule.next_after(due);
                            if due > now {
                                break;
                            }
                        }
                        reminder.due_at = due;
                    }
                    reminder.last_fired_at = Some(now);
                    fired.push((*id, reminder.due_at, title.clone()));
                    dirty = true;
                }
            }
            if dirty {
                changed.push(*id);
            }
        }

        for id in changed {
            if let Some(shared) = self.inner.notes.borrow().get(&id) {
                let body = shared.body.borrow().clone();
                let mut note = shared.note.borrow_mut();
                if let Err(err) = self.inner.store.save(&mut note, &body) {
                    eprintln!("failed to save reminder state for note {id}: {err}");
                }
            }
            self.schedule_commit();
        }
        for (id, due, title) in fired {
            self.notify_reminder(id, due, title);
        }
        self.refresh_tray_snapshot();
    }

    /// Fire a desktop notification with Open Note / Snooze / Done /
    /// Dismiss actions. If the notification hides on its own with no
    /// action taken, the reminder is snoozed for five minutes so it
    /// resurfaces (spec §3.4).
    fn notify_reminder(&self, id: Uuid, due: DateTime<Utc>, note_title: String) {
        let body = if note_title.is_empty() {
            "A note reminder is due".to_owned()
        } else {
            note_title
        };
        let tx = self.inner.tx.clone();
        match notify_rust::Notification::new()
            .appname("Pinlet")
            .summary("Reminder")
            .body(&body)
            .action("open", "Open Note")
            .action("snooze", "Snooze 10m")
            .action("done", "Mark done")
            .action("dismiss", "Dismiss")
            .timeout(notify_rust::Timeout::Milliseconds(10_000))
            .show()
        {
            Ok(handle) => {
                // wait_for_action blocks, so wait off the main loop.
                std::thread::spawn(move || {
                    handle.wait_for_action(|action| match action {
                        "open" => {
                            let _ = tx.send(Msg::FocusNote(id));
                        }
                        "snooze" => {
                            let _ = tx.send(Msg::Snooze {
                                note: id,
                                due,
                                minutes: 10,
                            });
                        }
                        "done" => {
                            let _ = tx.send(Msg::MarkDone { note: id, due });
                        }
                        "dismiss" => {
                            // Explicit dismissal: do not reschedule.
                        }
                        // "__closed": the notification hid without an action
                        // (timed out). Snooze briefly so it resurfaces.
                        _ => {
                            let _ = tx.send(Msg::Snooze {
                                note: id,
                                due,
                                minutes: 5,
                            });
                        }
                    });
                });
            }
            Err(err) => eprintln!("notification failed: {err}"),
        }
    }

    /// Add a reminder to a note.
    fn add_reminder(&self, id: Uuid, due: DateTime<Utc>, recurrence: Recurrence) {
        if let Some(shared) = self.inner.notes.borrow().get(&id) {
            shared.note.borrow_mut().reminders.push(Reminder {
                due_at: due,
                recurrence_rule: recurrence,
                last_fired_at: None,
            });
        }
        self.save_now(id);
    }

    /// Remove reminder `index` from a note.
    fn delete_reminder(&self, id: Uuid, index: usize) {
        if let Some(shared) = self.inner.notes.borrow().get(&id) {
            let mut note = shared.note.borrow_mut();
            if index < note.reminders.len() {
                note.reminders.remove(index);
            }
        }
        self.save_now(id);
    }

    /// Remove the fired reminder so it never fires again. The `due`
    /// identifies the reminder the notification fired for; for a
    /// recurring reminder that is the already-advanced next
    /// occurrence, so marking done ends the whole series.
    fn resolve_reminder(&self, note_id: Uuid, due: DateTime<Utc>) {
        let removed = {
            let Some(shared) = self.inner.notes.borrow().get(&note_id).cloned() else {
                return;
            };
            let mut note = shared.note.borrow_mut();
            note.reminders
                .iter()
                .position(|reminder| reminder.due_at == due)
                .map(|index| note.reminders.remove(index))
                .is_some()
        };
        if removed {
            self.save_now(note_id);
        }
    }

    /// Snooze the reminder with this due time by `minutes`.
    fn snooze_reminder(&self, note_id: Uuid, due: DateTime<Utc>, minutes: i64) {
        let changed = {
            let Some(shared) = self.inner.notes.borrow().get(&note_id).cloned() else {
                return;
            };
            let mut note = shared.note.borrow_mut();
            note.reminders
                .iter_mut()
                .find(|reminder| reminder.due_at == due)
                .is_some_and(|reminder| {
                    reminder.due_at = Utc::now() + ChronoDuration::minutes(minutes);
                    true
                })
        };
        if changed {
            self.save_now(note_id);
        }
    }

    /// Keep the tray menu's upcoming-reminders, notes, and trash
    /// sections current. Trashed notes stay out of the notes and
    /// reminders lists; they appear only in the trash section.
    fn refresh_tray_snapshot(&self) {
        let now = Utc::now();
        let mut upcoming = Vec::new();
        let mut notes = Vec::new();
        let mut trash = Vec::new();
        for shared in self.inner.notes.borrow().values() {
            let note = shared.note.borrow();
            let title = if note.is_locked {
                "🔒 Locked note".to_owned()
            } else {
                Note::derive_title(&shared.body.borrow())
            };
            if note.is_trashed {
                trash.push(crate::tray::TrayNote { id: note.id, title });
                continue;
            }
            notes.push(crate::tray::TrayNote {
                id: note.id,
                title: title.clone(),
            });
            for reminder in &note.reminders {
                let not_fired = reminder
                    .last_fired_at
                    .is_none_or(|last| last < reminder.due_at);
                if reminder.due_at >= now && not_fired {
                    upcoming.push(crate::tray::DueReminder {
                        note: note.id,
                        title: title.clone(),
                        due: reminder.due_at,
                    });
                }
            }
        }
        upcoming.sort_by_key(|reminder| reminder.due);
        upcoming.truncate(5);
        crate::tray::sort_by_title(&mut notes);
        crate::tray::sort_by_title(&mut trash);
        if let Ok(mut snapshot) = self.inner.tray_snapshot.lock() {
            snapshot.upcoming = upcoming;
            snapshot.notes = notes;
            snapshot.trash = trash;
        }
    }

    /// Open (or focus) the preferences window.
    fn open_settings(&self) {
        if self.inner.settings_window.borrow().is_none() {
            let (shortcut_support, shortcut_subtitle) = match self.inner.shortcut_backend {
                ShortcutBackend::Portal => (
                    true,
                    "Create a note from anywhere (configure the combo in system settings)",
                ),
                ShortcutBackend::GnomeSettings => (
                    true,
                    "Create a note from anywhere (configure the combo in Keyboard → Custom Shortcuts)",
                ),
                ShortcutBackend::None => (
                    false,
                    "Unavailable — requires the GlobalShortcuts portal (GNOME 47+) or a GNOME session",
                ),
            };
            let has_master_password =
                matches!(crate::secret::get_master_password(), Ok(Some(pw)) if !pw.is_empty());
            let window = SettingsWindow::new(
                &self.inner.gtk_app,
                &self.inner.settings.borrow(),
                self.inner.settings_path.clone(),
                has_master_password,
                shortcut_support,
                shortcut_subtitle,
                SettingsCallbacks {
                    on_force_global_color: Box::new({
                        let this = self.clone();
                        move |value| {
                            this.set_setting(|settings| settings.force_global_color = value);
                            this.apply_color_mode();
                        }
                    }),
                    on_default_color: Box::new({
                        let this = self.clone();
                        move |value| {
                            this.set_setting(|settings| settings.default_color = value);
                            this.apply_color_mode();
                        }
                    }),
                    on_sync_dark_mode: Box::new({
                        let this = self.clone();
                        move |value| this.set_setting(|settings| settings.sync_dark_mode = value)
                    }),
                    on_auto_save_debounce: Box::new({
                        let this = self.clone();
                        move |value| {
                            this.set_setting(|settings| settings.auto_save_debounce_ms = value)
                        }
                    }),
                    on_global_font_scale: Box::new({
                        let this = self.clone();
                        move |value| {
                            this.set_setting(|settings| settings.font_scale = value);
                            this.apply_font_mode();
                        }
                    }),
                    on_enable_shortcut: Box::new({
                        let this = self.clone();
                        move |value| {
                            this.set_setting(|settings| settings.enable_global_shortcut = value);
                            // The portal listener thread honors this gate
                            // live; spawn it on first enable (it can only
                            // be spawned once). The GNOME fallback
                            // registers/unregisters live.
                            this.inner.shortcut_gate.store(value, Ordering::SeqCst);
                            match this.inner.shortcut_backend {
                                ShortcutBackend::Portal => {
                                    if value && !*this.inner.shortcut_spawned.borrow() {
                                        crate::shortcuts::spawn(
                                            this.inner.tx.clone(),
                                            this.inner.shortcut_gate.clone(),
                                        );
                                        *this.inner.shortcut_spawned.borrow_mut() = true;
                                    }
                                }
                                ShortcutBackend::GnomeSettings => {
                                    if value {
                                        crate::gnome_shortcut::enable();
                                    } else {
                                        crate::gnome_shortcut::disable();
                                    }
                                }
                                ShortcutBackend::None => {}
                            }
                        }
                    }),
                    on_autostart: Box::new({
                        let this = self.clone();
                        move |value| {
                            this.set_setting(|settings| settings.autostart = value);
                            this.set_autostart(value);
                        }
                    }),
                    on_git_sync: Box::new({
                        let this = self.clone();
                        move |value| {
                            this.set_setting(|settings| settings.git_sync_enabled = value);
                            this.sync_settings_changed();
                        }
                    }),
                    on_git_remote: Box::new({
                        let this = self.clone();
                        move |value| {
                            this.set_setting(|settings| settings.git_remote_url = value);
                            this.sync_settings_changed();
                        }
                    }),
                    on_git_branch: Box::new({
                        let this = self.clone();
                        move |value| {
                            this.set_setting(|settings| settings.git_branch = value);
                            this.sync_settings_changed();
                        }
                    }),
                    on_git_commit_interval: Box::new({
                        let this = self.clone();
                        move |value| {
                            this.set_setting(|settings| settings.git_commit_interval_min = value);
                            this.sync_settings_changed();
                        }
                    }),
                    on_git_push_interval: Box::new({
                        let this = self.clone();
                        move |value| {
                            this.set_setting(|settings| settings.git_push_interval_min = value);
                            this.sync_settings_changed();
                        }
                    }),
                    on_git_pull: Box::new({
                        let this = self.clone();
                        move || {
                            let _ = this.inner.tx.send(Msg::GitPull);
                        }
                    }),
                    on_git_push: Box::new({
                        let this = self.clone();
                        move || {
                            let _ = this.inner.tx.send(Msg::GitPush);
                        }
                    }),
                    on_master_password: Box::new({
                        move |password| {
                            // Explicit Set-button semantics from the
                            // preferences UI: empty clears the secret.
                            // The outcome travels back so the UI can show
                            // it instead of failing silently.
                            if password.is_empty() {
                                crate::secret::clear_master_password()
                            } else {
                                crate::secret::set_master_password(&password)
                            }
                            .map_err(|err| err.to_string())
                        }
                    }),
                },
            );
            {
                let this = self.clone();
                window.connect_close(move || {
                    this.inner.settings_window.borrow_mut().take();
                });
            }
            *self.inner.settings_window.borrow_mut() = Some(window);
        }
        if let Some(window) = self.inner.settings_window.borrow().as_ref() {
            window.present();
        }
    }

    /// Write or remove the XDG autostart entry.
    fn set_autostart(&self, enabled: bool) {
        let dir = glib::user_config_dir().join("autostart");
        let path = dir.join("org.pinlet.Pinlet.desktop");
        if enabled {
            let exe = std::env::current_exe()
                .map_or_else(|_| "pinlet".to_owned(), |path| path.display().to_string());
            // Quote the executable: install paths may contain spaces.
            let entry = format!(
                "[Desktop Entry]\nType=Application\nName=Pinlet\n\
                 Comment=Sticky notes for your desktop\nExec=\"{exe}\"\n\
                 Terminal=false\nX-GNOME-Autostart-enabled=true\n"
            );
            if let Err(err) =
                std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, entry))
            {
                eprintln!("failed to write autostart entry: {err}");
            }
        } else if path.exists()
            && let Err(err) = std::fs::remove_file(&path)
        {
            eprintln!("failed to remove autostart entry: {err}");
        }
    }

    /// Mutate and persist one setting.
    fn set_setting(&self, apply: impl FnOnce(&mut Settings)) {
        let mut settings = self.inner.settings.borrow_mut();
        apply(&mut settings);
        if let Err(err) = settings.save(&self.inner.settings_path) {
            eprintln!("failed to save settings: {err}");
        }
    }

    /// Restyle every window per the color mode (spec Mode A / Mode B).
    fn apply_color_mode(&self) {
        let force = self.inner.settings.borrow().force_global_color;
        let global = self.default_color();
        for (id, window) in self.inner.windows.borrow().iter() {
            let color = if force {
                global.clone()
            } else if let Some(shared) = self.inner.notes.borrow().get(id) {
                shared.note.borrow().color.clone()
            } else {
                continue;
            };
            window.apply_color(&color);
        }
    }

    /// Restyle one window per the color mode. Used when opening a
    /// window, so startup stays linear instead of re-styling every
    /// window for each note.
    fn apply_color_for(&self, id: Uuid, window: &NoteWindow) {
        let force = self.inner.settings.borrow().force_global_color;
        let color = if force {
            self.default_color()
        } else if let Some(shared) = self.inner.notes.borrow().get(&id) {
            shared.note.borrow().color.clone()
        } else {
            return;
        };
        window.apply_color(&color);
    }

    /// Debounce window-geometry saves.
    fn geometry_changed(&self) {
        if let Some(source) = self.inner.geometry_source.borrow_mut().take() {
            cancel_source(source);
        }
        let this = self.clone();
        let source = glib::timeout_add_local_once(GEOMETRY_DEBOUNCE, move || this.save_geometry());
        *self.inner.geometry_source.borrow_mut() = Some(source);
    }

    /// Record every window's geometry into the local state file.
    fn save_geometry(&self) {
        if let Some(source) = self.inner.geometry_source.borrow_mut().take() {
            cancel_source(source);
        }
        for (id, window) in self.inner.windows.borrow().iter() {
            let (x, y) = if window.is_pinned() {
                window.pinned_position()
            } else {
                window.position_on_screen()
            };
            self.inner.local_state.borrow_mut().window_geometry.insert(
                id.to_string(),
                WindowGeometry {
                    x: x as i32,
                    y: y as i32,
                    width: window.width(),
                    height: window.height(),
                },
            );
        }
        if let Err(err) = self
            .inner
            .local_state
            .borrow()
            .save(&self.inner.local_state_path)
        {
            eprintln!("failed to save local state: {err}");
        }
    }

    /// Persist every note and commit before quitting. Files go out
    /// one by one, but the tray refresh and the commit happen once.
    fn flush_all(&self) {
        let ids: Vec<Uuid> = self.inner.notes.borrow().keys().copied().collect();
        for id in ids {
            self.persist_note(id);
        }
        if self.inner.commit_message.borrow().is_empty() {
            *self.inner.commit_message.borrow_mut() = "Save all notes".to_owned();
        }
        self.refresh_tray_snapshot();
        self.save_geometry();
        if let Err(err) = self
            .inner
            .local_state
            .borrow()
            .save(&self.inner.local_state_path)
        {
            eprintln!("failed to save local state: {err}");
        }
        self.commit_now();
    }
}

/// Run the sync git sequence (pull → commit → push) and report the
/// resulting state. A fast-forward-only pull runs first; on a diverged
/// remote the local changes are committed, the remote is merged, and the
/// push is retried, surfacing a merge conflict rather than a hard error.
fn run_sync(repo: &GitRepo, url: &str, branch: &str, commit_message: &str) -> SyncState {
    let fallback = if commit_message.is_empty() {
        "Sync"
    } else {
        commit_message
    };

    if let Err(err) = repo.configure_sync(url, branch) {
        return SyncState::Error(err.to_string());
    }

    // Pull fast-forwardable changes first; a failure here is expected on a
    // fresh remote (no branch yet) or a divergence, both handled below.
    let _ = repo.pull(branch);

    // A conflict from a previous sync still waits in the tree: report
    // it instead of failing the commit below and flapping to Error.
    if matches!(repo.has_conflicts(), Ok(true)) {
        return SyncState::Conflict;
    }

    if let Err(err) = repo.commit_all(fallback) {
        return SyncState::Error(err.to_string());
    }

    match repo.push(branch) {
        Ok(()) => SyncState::InSync,
        Err(_) => match repo.pull_merge(branch) {
            Ok(()) => match repo.push(branch) {
                Ok(()) => SyncState::InSync,
                Err(err) => SyncState::Error(err.to_string()),
            },
            Err(merge_err) => match repo.has_conflicts() {
                Ok(true) => SyncState::Conflict,
                _ => SyncState::Error(merge_err.to_string()),
            },
        },
    }
}

/// Human-readable status for the preferences window.
fn sync_state_text(state: &SyncState) -> String {
    match state {
        SyncState::Unconfigured => "Sync is not set up — configure a Remote URL.".to_owned(),
        SyncState::Syncing => "Syncing…".to_owned(),
        SyncState::InSync => "Notes are in sync.".to_owned(),
        SyncState::Conflict => "Merge conflict — resolve it in the note repository.".to_owned(),
        SyncState::Error(err) => format!("Sync failed: {err}"),
    }
}

/// The pinlet data directory: the XDG data dir + `pinlet`.
pub fn data_dir() -> PathBuf {
    glib::user_data_dir().join(DATA_DIR_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GTK can only be initialized once per process, and glib 0.20's
    /// default main context becomes owned by the first thread that
    /// iterates it — so all GTK-dependent regression scenarios run
    /// inside this ONE test function, sequentially on one thread.
    #[test]
    fn gtk_regressions() {
        gtk4::init().expect("GTK must initialize (needs a display)");

        note_body_text_gets_its_color();
        delete_note_scenario();
        picking_two_colors_scenario();
        pinning_scenario();
        markdown_source_roundtrip();
        enter_continuation_scenario();
    }

    /// Enter continuation through the real buffer wiring: task,
    /// bullet, and ordered items continue; empty items collapse.
    /// The continuation runs on idle, so the loop is pumped before
    /// asserting. GTK warnings raised mid-scenario print to stderr,
    /// so a regression here is visible in the test output.
    fn enter_continuation_scenario() {
        let buffer = gtk4::TextBuffer::new(None);
        crate::ui::wire_list_continuation(&buffer);
        let press_enter = |before: &str| {
            buffer.set_text(before);
            let end = buffer.end_iter();
            buffer.place_cursor(&end);
            let mut at = buffer.end_iter();
            buffer.insert(&mut at, "\n");
            while gtk4::glib::MainContext::default().iteration(false) {}
            buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), true)
                .to_string()
        };
        assert_eq!(press_enter("- [ ] buy milk"), "- [ ] buy milk\n- [ ] ");
        assert_eq!(press_enter("- item"), "- item\n- ");
        assert_eq!(press_enter("1. first"), "1. first\n2. ");
        assert_eq!(press_enter("- [ ]"), "");
        assert_eq!(press_enter("plain"), "plain\n");

        // Typing ahead of the idle still lands inside the continued item.
        buffer.set_text("- [ ] buy");
        let end = buffer.end_iter();
        buffer.place_cursor(&end);
        let mut at = buffer.end_iter();
        buffer.insert(&mut at, "\n");
        let mut at = buffer.end_iter();
        buffer.insert(&mut at, "more");
        while gtk4::glib::MainContext::default().iteration(false) {}
        assert_eq!(
            buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), true)
                .to_string(),
            "- [ ] buy\n- [ ] more"
        );
    }

    /// The Markdown styler must keep the hidden formatting markers in the
    /// buffer's canonical source. Reading with `include_hidden_chars = false`
    /// drops them (regression).
    fn markdown_source_roundtrip() {
        let buffer = gtk4::TextBuffer::new(None);
        buffer.set_text("## Header\n\n**bold** and *italic*");
        let styler = crate::markdown::MarkdownStyler::new(&buffer);
        styler.restyle(&buffer);

        let start = buffer.start_iter();
        let end = buffer.end_iter();
        assert_eq!(
            buffer.text(&start, &end, true).to_string(),
            "## Header\n\n**bold** and *italic*"
        );
        let visible = buffer.text(&start, &end, false).to_string();
        assert!(!visible.contains("##"));
        assert!(!visible.contains("**"));
    }

    /// Toggling desktop pinning recreates the note as a background
    /// layer window (regression test for the pinning feature).
    fn pinning_scenario() {
        if !gtk4_layer_shell::is_supported() && !crate::ui::x11::is_supported() {
            eprintln!("no pinning mechanism available — skipping pin scenario");
            return;
        }
        let scratch = std::env::temp_dir().join(format!("pinlet-pin-test-{}", Uuid::new_v4()));

        let gtk_app = gtk4::Application::builder()
            .application_id("org.pinlet.TestPin")
            .build();
        let app = App::new_in(&gtk_app, scratch.join("pinlet")).expect("app initializes");
        let id = app.new_note(NoteColor::Yellow, "pin me".to_owned());

        app.toggle_pin(id);
        let pinned = app
            .inner
            .notes
            .borrow()
            .get(&id)
            .is_some_and(|shared| shared.note.borrow().is_pinned_to_desktop);
        assert!(pinned, "note should be marked pinned");
        let window = app.inner.windows.borrow().get(&id).unwrap().clone();
        assert!(
            window.is_pinned(),
            "window should live on the desktop layer"
        );
        assert!(
            window.is_layer_window() || window.is_desktop_window(),
            "window must be on a compositor layer or an X11 desktop window"
        );
        pump_main_loop(); // let the pinned window realize and map
        assert!(
            window.window().is_visible(),
            "pinned window should be visible"
        );

        app.toggle_pin(id);
        let pinned = app
            .inner
            .notes
            .borrow()
            .get(&id)
            .is_some_and(|shared| shared.note.borrow().is_pinned_to_desktop);
        assert!(!pinned, "note should be unpinned again");
        let window = app.inner.windows.borrow().get(&id).unwrap().clone();
        assert!(
            !window.is_pinned(),
            "window should be a regular window again"
        );

        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// The note body must resolve the per-color foreground even when
    /// the theme's own `textview` color says otherwise (regression
    /// test for the unreadable-body-text bug).
    fn note_body_text_gets_its_color() {
        crate::ui::ensure_styles();
        let window = gtk4::Window::builder().build();
        window.set_css_classes(&["pinlet-yellow"]);
        let view = gtk4::TextView::new();
        view.add_css_class("pinlet-body");
        window.set_child(Some(&view));
        gtk4::prelude::WidgetExt::realize(&window);

        // #4a4523, the yellow note's pinned foreground.
        let expected = gtk4::gdk::RGBA::new(
            0x4a as f32 / 255.0,
            0x45 as f32 / 255.0,
            0x23 as f32 / 255.0,
            1.0,
        );
        let resolved = view.style_context().color();
        assert!(
            (resolved.red() - expected.red()).abs() < 0.01
                && (resolved.green() - expected.green()).abs() < 0.01
                && (resolved.blue() - expected.blue()).abs() < 0.01,
            "note body resolved {:?}, expected {:?}",
            resolved,
            expected
        );
    }

    /// Deleting a note closes its window while the close-request
    /// handler touches the registries — this must not double-borrow
    /// (regression test for the delete crash).
    fn delete_note_scenario() {
        let scratch = std::env::temp_dir().join(format!("pinlet-app-test-{}", Uuid::new_v4()));

        let gtk_app = gtk4::Application::builder()
            .application_id("org.pinlet.Test")
            .build();
        let app = App::new_in(&gtk_app, scratch.join("pinlet")).expect("app initializes");
        let id = app.new_note(NoteColor::Yellow, "delete me".to_owned());
        assert!(app.inner.notes.borrow().contains_key(&id));

        app.delete_note(id);
        assert!(app.inner.notes.borrow().is_empty());
        assert!(!app.inner.store.path_for(id).exists());

        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// Clicking two palette swatches in a row drives the full
    /// change→debounced-save chain twice — this must not crash
    /// (regression test for the color picker crash).
    fn picking_two_colors_scenario() {
        let scratch = std::env::temp_dir().join(format!("pinlet-colors-test-{}", Uuid::new_v4()));

        let gtk_app = gtk4::Application::builder()
            .application_id("org.pinlet.TestColors")
            .build();
        let app = App::new_in(&gtk_app, scratch.join("pinlet")).expect("app initializes");
        let id = app.new_note(NoteColor::Yellow, "color me".to_owned());
        let window = app.inner.windows.borrow().get(&id).unwrap().clone();

        // Walk to the palette swatches: titlebar → color MenuButton →
        // Popover → FlowBox → Buttons. Matched by tooltip: the header
        // holds several menu buttons (reminders, color), and the first
        // in tree order is not the color one.
        let header = window.window().titlebar().expect("titlebar");
        let menu_button = find_menu_button(&header, "Note color").expect("color menu button");
        let popover = menu_button.popover().expect("popover");
        let flow = popover
            .child()
            .expect("popover child")
            .downcast::<gtk4::FlowBox>()
            .expect("flow box");
        // FlowBox wraps each child in a FlowBoxChild.
        let mut swatches = Vec::new();
        let mut child = flow.first_child();
        while let Some(widget) = child {
            if let Ok(wrapper) = widget.clone().downcast::<gtk4::FlowBoxChild>()
                && let Some(button) = wrapper
                    .child()
                    .and_then(|w| w.downcast::<gtk4::Button>().ok())
            {
                swatches.push(button);
            }
            child = widget.next_sibling();
        }
        assert_eq!(swatches.len(), 6, "six palette swatches");

        swatches[1].emit_by_name::<()>("clicked", &[]);
        pump_main_loop();
        let color = app
            .inner
            .notes
            .borrow()
            .get(&id)
            .unwrap()
            .note
            .borrow()
            .color
            .clone();
        assert_eq!(color, NoteColor::Green, "first pick applied");

        swatches[2].emit_by_name::<()>("clicked", &[]);
        pump_main_loop();
        let color = app
            .inner
            .notes
            .borrow()
            .get(&id)
            .unwrap()
            .note
            .borrow()
            .color
            .clone();
        assert_eq!(color, NoteColor::Blue, "second pick applied");

        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// Pump the main loop long enough for the debounced save
    /// (500 ms) to fire.
    fn pump_main_loop() {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(800);
        while std::time::Instant::now() < deadline {
            while gtk4::glib::MainContext::default().iteration(false) {}
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Depth-first search for the MenuButton with the given tooltip in
    /// a widget tree.
    fn find_menu_button(widget: &gtk4::Widget, tooltip: &str) -> Option<gtk4::MenuButton> {
        if let Ok(button) = widget.clone().downcast::<gtk4::MenuButton>() {
            if button.tooltip_text().as_deref() == Some(tooltip) {
                return Some(button);
            }
        }
        let mut child = widget.first_child();
        while let Some(next) = child {
            if let Some(found) = find_menu_button(&next, tooltip) {
                return Some(found);
            }
            child = next.next_sibling();
        }
        None
    }
}
