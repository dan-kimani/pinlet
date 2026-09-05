//! Pinlet — native sticky notes for Linux desktops.
//!
//! Notes are plain Markdown files (YAML frontmatter + body) inside a
//! git repository the user fully owns. See `docs/spec.md` for the
//! full design.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod app;
pub mod cli;
pub mod error;
pub mod messages;
pub mod search;
pub mod settings;
pub mod storage;
pub mod tray;
pub mod ui;
