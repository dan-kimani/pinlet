//! Application core: note store, git repository, settings, tray,
//! reminder engine, and the open-window registry. The GTK layer
//! (see [`crate::ui`]) is driven from here.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use gtk4::gio;
use gtk4::glib::{self, ControlFlow, SourceId};
use gtk4::prelude::*;
use uuid::Uuid;

use crate::cli;
use crate::error::AppResult;
use crate::messages::Msg;
use crate::search::SearchIndex;
use crate::settings::{Settings, SETTINGS_FILE};
use crate::storage::{
    GitRepo, LocalState, Note, NoteColor, NoteStore, Recurrence, Reminder, WindowGeometry,
    DATA_DIR_NAME, LOCAL_STATE_FILE,
};
use crate::tray::{PinletTray, TraySnapshot};
use crate::ui::{NoteCallbacks, NoteWindow, SearchWindow};

/// State shared between one note window and the app core.
pub struct SharedNote {
    /// Frontmatter metadata.
    pub note: RefCell<Note>,
    /// Markdown body.
    pub body: RefCell<String>,
}

impl SharedNote {
    /// Window-friendly title: the derived title, or a fallback.
    pub fn display_title(&self) -> String {
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

struct AppInner {
    gtk_app: gtk4::Application,
    store: NoteStore,
    repo: GitRepo,
    settings: Settings,
    /// Every live note, keyed by id.
    notes: RefCell<HashMap<Uuid, Rc<SharedNote>>>,
    /// Open window per note id.
    windows: RefCell<HashMap<Uuid, NoteWindow>>,
    /// Pending debounced save timers, one per note.
    save_sources: RefCell<HashMap<Uuid, SourceId>>,
    /// Pending coalesced auto-commit timer.
    commit_source: RefCell<Option<SourceId>>,
    /// Message for the next auto-commit; the last change wins.
    commit_message: RefCell<String>,
    /// Reminder-check tick loop.
    tick_source: RefCell<Option<SourceId>>,
    /// Debounced geometry-save timer.
    geometry_source: RefCell<Option<SourceId>>,
    /// In-memory search index, kept in sync on save/delete.
    search_index: RefCell<SearchIndex>,
    /// Machine-local state (git-ignored).
    local_state: RefCell<LocalState>,
    local_state_path: PathBuf,
    /// The single quick-find window, created lazily.
    search: RefCell<Option<Rc<SearchWindow>>>,
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
    /// repo, spawn the tray, and attach the message channel.
    pub fn new(gtk_app: &gtk4::Application) -> AppResult<Self> {
        let data_dir = data_dir()?;
        let store = NoteStore::open(&data_dir)?;
        let repo = GitRepo::open_or_init(data_dir.clone())?;
        let settings = Settings::load(&data_dir.join(SETTINGS_FILE))?;
        let local_state_path = data_dir.join(LOCAL_STATE_FILE);
        let local_state = LocalState::load(&local_state_path)?;

        let mut notes = HashMap::new();
        let mut search_index = SearchIndex::default();
        for (note, body) in store.load_all()? {
            search_index.update(note.id, &note.title, &body);
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
            store,
            repo,
            settings,
            notes: RefCell::new(notes),
            windows: RefCell::new(HashMap::new()),
            save_sources: RefCell::new(HashMap::new()),
            commit_source: RefCell::new(None),
            commit_message: RefCell::new(String::new()),
            tick_source: RefCell::new(None),
            geometry_source: RefCell::new(None),
            search_index: RefCell::new(search_index),
            local_state: RefCell::new(local_state),
            local_state_path,
            search: RefCell::new(None),
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
        let ids: Vec<Uuid> = self.inner.notes.borrow().keys().copied().collect();
        for id in ids {
            self.open_window(id);
        }

        match &cli.command {
            Some(cli::Command::New { text, color }) => {
                let color = color
                    .as_deref()
                    .and_then(|raw| raw.parse().ok())
                    .unwrap_or_else(|| self.default_color());
                self.new_note(color, text.clone().unwrap_or_default());
            }
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
        self.inner.tick_source.replace(Some(glib::timeout_add_local(
            TICK_INTERVAL,
            move || {
                this.check_due_reminders();
                ControlFlow::Continue
            },
        )));
        self.refresh_tray_snapshot();
    }

    /// The default color for new notes, from settings.
    fn default_color(&self) -> NoteColor {
        self.inner
            .settings
            .default_color
            .parse()
            .expect("NoteColor parsing is infallible")
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
            let action = gio::SimpleAction::new("search", None);
            action.connect_activate(move |_, _| this.open_search());
            app.add_action(&action);
            app.set_accels_for_action("app.search", &["<Primary><Shift>f"]);
        }
        {
            let this = self.clone();
            let action = gio::SimpleAction::new("quit", None);
            action.connect_activate(move |_, _| {
                this.flush_all();
                this.inner.gtk_app.quit();
            });
            app.add_action(&action);
            app.set_accels_for_action("app.quit", &["<Primary>q"]);
        }
    }

    /// Handle a message from the tray or a notification action.
    fn handle_msg(&self, msg: Msg) {
        match msg {
            Msg::NewNote => {
                self.new_note(self.default_color(), String::new());
            }
            Msg::FocusNote(id) => self.focus_note(id),
            Msg::ToggleAll => self.toggle_all(),
            Msg::Search => self.open_search(),
            Msg::Snooze { note, due, minutes } => self.snooze_reminder(note, due, minutes),
            Msg::Quit => {
                self.flush_all();
                self.inner.gtk_app.quit();
            }
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

    /// Open a window for note `id`, wiring its callbacks.
    fn open_window(&self, id: Uuid) {
        let shared = self
            .inner
            .notes
            .borrow()
            .get(&id)
            .expect("note exists")
            .clone();

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
                move || this.delete_note(id)
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
                    this.commit_now();
                }
            }),
            on_add_reminder: Box::new({
                let this = self.clone();
                move |due| this.add_reminder(id, due)
            }),
            on_delete_reminder: Box::new({
                let this = self.clone();
                move |index| this.delete_reminder(id, index)
            }),
            on_geometry_changed: Box::new({
                let this = self.clone();
                move || this.geometry_changed()
            }),
        };

        let window = NoteWindow::new(&self.inner.gtk_app, shared.clone(), geometry, callbacks);
        self.inner.windows.borrow_mut().insert(id, window.clone());
        window.present();
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

    /// Remove the note from disk and from the registry.
    fn delete_note(&self, id: Uuid) {
        if let Some(shared) = self.inner.notes.borrow_mut().remove(&id) {
            let title = shared.display_title();
            if let Some(window) = self.inner.windows.borrow_mut().remove(&id) {
                window.close();
            }
            self.inner
                .local_state
                .borrow_mut()
                .window_geometry
                .remove(&id.to_string());
            self.inner.search_index.borrow_mut().remove(id);
            if let Err(err) = self.inner.store.delete(id) {
                eprintln!("failed to delete note {id}: {err}");
            }
            if let Some(source) = self.inner.save_sources.borrow_mut().remove(&id) {
                source.remove();
            }
            *self.inner.commit_message.borrow_mut() = format!("Delete note '{title}'");
            self.schedule_commit();
            self.refresh_tray_snapshot();
        }
    }

    /// Replace the note body and (re)start the debounced save timer.
    fn debounced_save(&self, id: Uuid, content: String) {
        if let Some(shared) = self.inner.notes.borrow().get(&id) {
            *shared.body.borrow_mut() = content;
        }
        if let Some(source) = self.inner.save_sources.borrow_mut().remove(&id) {
            source.remove();
        }
        let delay = Duration::from_millis(self.inner.settings.auto_save_debounce_ms);
        let this = self.clone();
        let source = glib::timeout_add_local_once(delay, move || this.save_now(id));
        self.inner.save_sources.borrow_mut().insert(id, source);
    }

    /// Persist the note, refresh the index and tray, and schedule an
    /// auto-commit.
    fn save_now(&self, id: Uuid) {
        let Some(shared) = self.inner.notes.borrow().get(&id).cloned() else {
            return;
        };
        let body = shared.body.borrow().clone();
        let mut note = shared.note.borrow_mut();
        note.title = Note::derive_title(&body);
        if let Err(err) = self.inner.store.save(&mut note, &body) {
            eprintln!("failed to save note {id}: {err}");
            return;
        }
        let title = note.title.clone();
        drop(note);
        self.inner.search_index.borrow_mut().update(id, &title, &body);
        let display = shared.display_title();
        *self.inner.commit_message.borrow_mut() = format!("Update '{display}'");
        self.schedule_commit();
        self.refresh_tray_snapshot();
    }

    /// (Re)start the coalesced auto-commit timer.
    fn schedule_commit(&self) {
        if let Some(source) = self.inner.commit_source.borrow_mut().take() {
            source.remove();
        }
        let this = self.clone();
        let source = glib::timeout_add_local_once(AUTO_COMMIT_IDLE, move || this.commit_now());
        *self.inner.commit_source.borrow_mut() = Some(source);
    }

    /// Commit pending changes, if any.
    fn commit_now(&self) {
        if let Some(source) = self.inner.commit_source.borrow_mut().take() {
            source.remove();
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
            let title = note.title.clone();
            let mut dirty = false;
            for reminder in &mut note.reminders {
                let already = reminder
                    .last_fired_at
                    .is_some_and(|last| last >= reminder.due_at);
                if reminder.due_at <= now && !already {
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

    /// Fire a desktop notification with Open Note / Snooze actions
    /// (spec §3.4).
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
                        _ => {}
                    });
                });
            }
            Err(err) => eprintln!("notification failed: {err}"),
        }
    }

    /// Add a one-shot reminder to a note.
    fn add_reminder(&self, id: Uuid, due: DateTime<Utc>) {
        if let Some(shared) = self.inner.notes.borrow().get(&id) {
            shared.note.borrow_mut().reminders.push(Reminder {
                due_at: due,
                recurrence_rule: Recurrence::None,
                last_fired_at: None,
                snooze_count: 0,
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
                    reminder.snooze_count += 1;
                    true
                })
        };
        if changed {
            self.save_now(note_id);
        }
    }

    /// Keep the tray menu's upcoming-reminders section current.
    fn refresh_tray_snapshot(&self) {
        let now = Utc::now();
        let mut upcoming = Vec::new();
        for shared in self.inner.notes.borrow().values() {
            let note = shared.note.borrow();
            let title = Note::derive_title(&shared.body.borrow());
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
        if let Ok(mut snapshot) = self.inner.tray_snapshot.lock() {
            snapshot.upcoming = upcoming;
        }
    }

    /// Open (or focus) the quick-find window.
    fn open_search(&self) {
        if self.inner.search.borrow().is_none() {
            let window = SearchWindow::new(
                &self.inner.gtk_app,
                {
                    let this = self.clone();
                    move |query| {
                        let hits = this.inner.search_index.borrow().query(&query, 20);
                        if let Some(search) = this.inner.search.borrow().as_ref() {
                            search.set_results(&hits);
                        }
                    }
                },
                {
                    let this = self.clone();
                    move |id| this.focus_note(id)
                },
            );
            *self.inner.search.borrow_mut() = Some(window);
        }
        if let Some(search) = self.inner.search.borrow().as_ref() {
            search.present();
        }
    }

    /// Debounce window-geometry saves.
    fn geometry_changed(&self) {
        if let Some(source) = self.inner.geometry_source.borrow_mut().take() {
            source.remove();
        }
        let this = self.clone();
        let source =
            glib::timeout_add_local_once(GEOMETRY_DEBOUNCE, move || this.save_geometry());
        *self.inner.geometry_source.borrow_mut() = Some(source);
    }

    /// Record every window's geometry into the local state file.
    fn save_geometry(&self) {
        if let Some(source) = self.inner.geometry_source.borrow_mut().take() {
            source.remove();
        }
        for (id, window) in self.inner.windows.borrow().iter() {
            let (x, y) = window.position_on_screen();
            self.inner
                .local_state
                .borrow_mut()
                .window_geometry
                .insert(
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

    /// Persist every note and commit before quitting.
    fn flush_all(&self) {
        let ids: Vec<Uuid> = self.inner.notes.borrow().keys().copied().collect();
        for id in ids {
            self.save_now(id);
        }
        self.save_geometry();
        self.commit_now();
    }
}

/// The pinlet data directory: the XDG data dir + `pinlet`.
pub fn data_dir() -> AppResult<PathBuf> {
    Ok(glib::user_data_dir().join(DATA_DIR_NAME))
}
