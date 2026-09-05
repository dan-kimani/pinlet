//! Filesystem-backed note storage.
//!
//! Each note is a Markdown file — YAML frontmatter plus body — inside
//! a git repository the user fully owns (spec §4).

mod model;
mod repo;
mod store;

pub use model::{parse_note_file, render_file, Note, NoteColor, Recurrence, Reminder};
pub use repo::GitRepo;
pub use store::{NoteStore, DATA_DIR_NAME};
