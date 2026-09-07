//! Atomic file writes.
//!
//! State files are written via temp-file + rename so a crash mid-write
//! can never leave a truncated file behind. Files holding secrets
//! additionally get owner-only permissions.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::error::AppResult;

/// Atomically write `contents` to `path` (temp file + rename).
pub fn atomic_write(path: &Path, contents: &[u8]) -> AppResult<()> {
    let tmp = tmp_path(path);
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Atomically write `contents` to `path`, restricting the file to
/// owner-only access (mode `600`).
///
/// Permissions are set on the temp file before the rename, so the
/// destination is never briefly world-readable — and an existing file
/// with lax permissions heals itself on the next save.
pub fn atomic_write_private(path: &Path, contents: &[u8]) -> AppResult<()> {
    let tmp = tmp_path(path);
    fs::write(&tmp, contents)?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Temp path alongside `path`: `{path}.tmp`, so it lands on the same
/// filesystem (and the rename stays atomic).
fn tmp_path(path: &Path) -> PathBuf {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    PathBuf::from(tmp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A unique scratch path for one test.
    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "pinlet-fs-test-{}-{}-{}.json",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst),
            name
        ))
    }

    #[test]
    fn plain_write_round_trips() {
        let path = scratch("plain");
        atomic_write(&path, b"hello").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn private_write_round_trips_with_owner_only_mode() {
        let path = scratch("private");
        atomic_write_private(&path, b"{\"a\":1}").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"a\":1}");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::remove_file(&path).unwrap();
    }
}
