//! Application core: note store, git repository, settings, and the
//! open-window registry. The GTK layer (see [`crate::ui`]) is driven
//! from here.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gtk4::glib::{self, SourceId};
use uuid::Uuid;

use crate::cli;
use crate::error::AppResult;
use crate::settings::{SETTINGS_FILE, Settings};
use crate::storage::{DATA_DIR_NAME, GitRepo, Note, NoteColor, NoteStore};
use crate::ui::NoteWindow;

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
    store: NoteStore,
    repo: GitRepo,
    settings: Settings,
    /// Every live note, keyed by id.
    notes: RefCell<HashMap<Uuid, Rc<SharedNote>>>,
    /// Pending debounced save timers, one per note.
    save_sources: RefCell<HashMap<Uuid, SourceId>>,
    /// Pending coalesced auto-commit timer.
    commit_source: RefCell<Option<SourceId>>,
    /// Message for the next auto-commit; the last change wins.
    commit_message: RefCell<String>,
}

/// Idle time before an auto-commit (spec §3.1).
const AUTO_COMMIT_IDLE: Duration = Duration::from_secs(120);

impl App {
    /// Load settings, open the note store, and prepare the git repo.
    pub fn new() -> AppResult<Self> {
        let data_dir = data_dir()?;
        let store = NoteStore::open(&data_dir)?;
        let repo = GitRepo::open_or_init(data_dir.clone())?;
        let settings = Settings::load(&data_dir.join(SETTINGS_FILE))?;

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

        Ok(Self {
            inner: Rc::new(AppInner {
                store,
                repo,
                settings,
                notes: RefCell::new(notes),
                save_sources: RefCell::new(HashMap::new()),
                commit_source: RefCell::new(None),
                commit_message: RefCell::new(String::new()),
            }),
        })
    }

    /// Open every note's window; honor the CLI quick-capture command.
    pub fn activate(&self, app: &gtk4::Application, cli: &cli::Cli) {
        let ids: Vec<Uuid> = self.inner.notes.borrow().keys().copied().collect();
        for id in ids {
            self.open_window(app, id);
        }

        match &cli.command {
            Some(cli::Command::New { text, color }) => {
                let color = color
                    .as_deref()
                    .and_then(|raw| raw.parse().ok())
                    .unwrap_or_else(|| self.default_color());
                self.new_note(app, color, text.clone().unwrap_or_default());
            }
            _ if self.inner.notes.borrow().is_empty() => {
                self.new_note(app, self.default_color(), String::new());
            }
            _ => {}
        }
    }

    /// The default color for new notes, from settings.
    fn default_color(&self) -> NoteColor {
        self.inner
            .settings
            .default_color
            .parse()
            .expect("NoteColor parsing is infallible")
    }

    /// Create a note (window included) and persist it immediately.
    fn new_note(&self, app: &gtk4::Application, color: NoteColor, text: String) -> Uuid {
        let id = Uuid::new_v4();
        let shared = Rc::new(SharedNote {
            note: RefCell::new(Note::new(id, color)),
            body: RefCell::new(text),
        });
        self.inner.notes.borrow_mut().insert(id, shared.clone());
        self.open_window(app, id);
        self.save_now(id);
        let title = shared.display_title();
        *self.inner.commit_message.borrow_mut() = format!("Create note '{title}'");
        self.schedule_commit();
        id
    }

    /// Open a window for note `id`, wiring its callbacks.
    fn open_window(&self, app: &gtk4::Application, id: Uuid) {
        let shared = self
            .inner
            .notes
            .borrow()
            .get(&id)
            .expect("note exists")
            .clone();

        let window = NoteWindow::new(
            app,
            shared.clone(),
            {
                let this = self.clone();
                move |content| this.debounced_save(id, content)
            },
            {
                let this = self.clone();
                move || this.delete_note(id)
            },
            {
                let this = self.clone();
                let app = app.clone();
                move || {
                    this.new_note(&app, this.default_color(), String::new());
                }
            },
            {
                let this = self.clone();
                move || {
                    this.save_now(id);
                    this.commit_now();
                }
            },
        );
        window.present();
    }

    /// Remove the note from disk and from the registry.
    fn delete_note(&self, id: Uuid) {
        if let Some(shared) = self.inner.notes.borrow_mut().remove(&id) {
            let title = shared.display_title();
            if let Err(err) = self.inner.store.delete(id) {
                eprintln!("failed to delete note {id}: {err}");
            }
            if let Some(source) = self.inner.save_sources.borrow_mut().remove(&id) {
                source.remove();
            }
            *self.inner.commit_message.borrow_mut() = format!("Delete note '{title}'");
            self.schedule_commit();
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

    /// Persist the note and schedule an auto-commit.
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
        drop(note);
        let title = shared.display_title();
        *self.inner.commit_message.borrow_mut() = format!("Update '{title}'");
        self.schedule_commit();
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
}

/// The pinlet data directory: the XDG data dir + `pinlet`.
pub fn data_dir() -> AppResult<PathBuf> {
    Ok(glib::user_data_dir().join(DATA_DIR_NAME))
}
