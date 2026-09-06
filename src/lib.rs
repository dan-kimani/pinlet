//! Pinlet — native sticky notes for Linux desktops.
//!
//! Notes are plain Markdown files (YAML frontmatter + body) inside a
//! git repository the user fully owns. See `docs/spec.md` for the
//! full design.

// unsafe_code is denied (not forbidden) so the one-line FFI shim in
// `ui::x11` can be allowed locally; everything else stays safe.
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod app;
pub mod cli;
pub mod error;
pub mod gnome_shortcut;
pub mod markdown;
pub mod messages;
pub mod pinning;
pub mod search;
pub mod settings;
pub mod shortcuts;
pub mod storage;
pub mod timer;
pub mod tray;
pub mod ui;
