//! Filesystem-backed note storage.
//!
//! Each note is a Markdown file — YAML frontmatter plus body — inside
//! a git repository the user fully owns (spec §4).

mod local_state;
mod model;
mod repo;
mod store;

pub mod crypto;

pub use local_state::{LOCAL_STATE_FILE, LocalState, WindowGeometry};
pub use model::{Note, NoteColor, Recurrence, Reminder, parse_note_file, render_file};
pub use repo::GitRepo;
pub use store::{DATA_DIR_NAME, NoteStore};
