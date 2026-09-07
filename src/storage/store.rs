//! Filesystem-backed note store: one Markdown file per note, with
//! atomic writes (temp file + rename).

use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;
use uuid::Uuid;

use super::model::{Note, parse_note_file, render_file};
use crate::error::AppResult;

/// Directory name under the XDG data dir; this is also the git repo.
pub const DATA_DIR_NAME: &str = "pinlet";

/// Subdirectory holding the note files.
const NOTES_DIR: &str = "notes";

/// Handles note persistence. Cheap to clone.
#[derive(Debug, Clone)]
pub struct NoteStore {
    root: PathBuf,
}

impl NoteStore {
    /// Open (or create) the store at `root`.
    pub fn open(root: impl Into<PathBuf>) -> AppResult<Self> {
        let store = Self { root: root.into() };
        fs::create_dir_all(store.notes_dir())?;
        fs::create_dir_all(store.locked_dir())?;
        Ok(store)
    }

    /// Directory holding locked (encrypted) note files. Git-ignored:
    /// locked notes never sync (spec §3.10).
    pub fn locked_dir(&self) -> PathBuf {
        self.root.join("locked")
    }

    /// Directory containing the note files.
    pub fn notes_dir(&self) -> PathBuf {
        self.root.join(NOTES_DIR)
    }

    /// Path of a note's file on disk.
    pub fn path_for(&self, id: Uuid) -> PathBuf {
        self.notes_dir().join(format!("{id}.md"))
    }

    /// Load every readable note paired with its body text — plain
    /// notes and locked notes (whose body is an encrypted blob).
    /// Malformed files are skipped with a warning, never fatal.
    pub fn load_all(&self) -> AppResult<Vec<(Note, String)>> {
        let mut notes = Vec::new();
        for dir in [self.notes_dir(), self.locked_dir()] {
            for entry in fs::read_dir(&dir)? {
                let path = entry?.path();
                if !is_note_file(&path) {
                    continue;
                }
                match parse_note_file(&path) {
                    Ok(pair) => notes.push(pair),
                    Err(err) => eprintln!("skipping unreadable note: {err}"),
                }
            }
        }
        notes.sort_by_key(|(note, _)| note.created_at);
        Ok(notes)
    }

    /// Atomically write the note file and bump `updated_at`. Locked
    /// notes are written to the git-ignored `locked/` directory; for
    /// them `body` is the encrypted blob.
    pub fn save(&self, note: &mut Note, body: &str) -> AppResult<()> {
        note.updated_at = Some(Utc::now());
        let rendered = render_file(note, body)?;
        let path = if note.is_locked {
            self.locked_dir().join(format!("{}.md", note.id))
        } else {
            self.path_for(note.id)
        };
        // The directories are created at open, but the user may delete
        // one mid-session — recreate rather than fail the save.
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        crate::fs::atomic_write(&path, rendered.as_bytes())
    }

    /// Remove a note's files — plain and locked — if they exist.
    pub fn delete(&self, id: Uuid) -> AppResult<()> {
        for path in [
            self.path_for(id),
            self.locked_dir().join(format!("{id}.md")),
        ] {
            if path.exists() {
                fs::remove_file(path)?;
            }
        }
        Ok(())
    }
}

/// Only plain note files count; temp files (`.md.tmp`) do not.
fn is_note_file(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()) == Some("md")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::NoteColor;

    /// A unique scratch directory for one test.
    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!("pinlet-test-{}", Uuid::new_v4()))
    }

    #[test]
    fn save_then_load_round_trips() {
        let root = temp_root();
        let store = NoteStore::open(&root).unwrap();

        let mut note = Note::new(Uuid::new_v4(), NoteColor::Green);
        let body = "# Shopping\n\n- [ ] Milk\n";
        store.save(&mut note, body).unwrap();

        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.len(), 1);
        let (loaded_note, loaded_body) = &loaded[0];
        assert_eq!(loaded_note.id, note.id);
        assert_eq!(loaded_note.color, NoteColor::Green);
        assert_eq!(loaded_body, body);
        assert!(loaded_note.updated_at.is_some());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn delete_removes_file() {
        let root = temp_root();
        let store = NoteStore::open(&root).unwrap();

        let mut note = Note::new(Uuid::new_v4(), NoteColor::Yellow);
        store.save(&mut note, "hello").unwrap();
        store.delete(note.id).unwrap();

        assert!(store.load_all().unwrap().is_empty());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn malformed_files_are_skipped() {
        let root = temp_root();
        let store = NoteStore::open(&root).unwrap();

        fs::write(store.notes_dir().join("broken.md"), "no frontmatter here").unwrap();

        let mut note = Note::new(Uuid::new_v4(), NoteColor::Yellow);
        store.save(&mut note, "fine").unwrap();

        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].0.id, note.id);

        fs::remove_dir_all(root).unwrap();
    }
}
