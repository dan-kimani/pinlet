//! Filesystem-backed note storage.
//!
//! Each note is a Markdown file — YAML frontmatter plus body — inside
//! a git repository the user fully owns (spec §4).

mod local_state;
mod model;
mod repo;
mod store;

pub mod crypto;

pub use local_state::{LocalState, WindowGeometry, LOCAL_STATE_FILE};
pub use model::{parse_note_file, render_file, Note, NoteColor, Recurrence, Reminder};
pub use repo::GitRepo;
pub use store::{NoteStore, DATA_DIR_NAME};
