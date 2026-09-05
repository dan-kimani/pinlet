//! Unified error type for the whole application.

use std::path::PathBuf;

use thiserror::Error;

/// All failures the application can produce.
#[derive(Debug, Error)]
pub enum AppError {
    /// Filesystem I/O failure.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// YAML (de)serialization failure.
    #[error("YAML error: {0}")]
    Yaml(#[from] serde_yaml_ng::Error),

    /// The `git` CLI failed.
    #[error("git failed: {0}")]
    Git(String),

    /// A note file is missing or has malformed frontmatter.
    #[error("invalid note file '{}': {}", path.display(), reason)]
    InvalidNoteFile {
        /// Path of the offending file.
        path: PathBuf,
        /// Why it could not be parsed.
        reason: String,
    },

    /// `settings.json` could not be read.
    #[error("settings error: {0}")]
    Settings(String),

    /// The XDG data directory could not be resolved.
    #[error("could not resolve the XDG data directory")]
    DataDir,
}

/// Convenience alias used throughout the crate.
pub type AppResult<T> = Result<T, AppError>;
