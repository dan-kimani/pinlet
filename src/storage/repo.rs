//! Git integration for the note repository.
//!
//! v1 shells out to the `git` CLI for auto-commits (zero extra
//! dependencies); Phase 4 swaps the internals for `gix` to add
//! built-in pull/push sync. The public interface stays the same.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{AppError, AppResult};

/// Handles version control of the note repository.
#[derive(Debug, Clone)]
pub struct GitRepo {
    dir: PathBuf,
}

impl GitRepo {
    /// Ensure `dir` is a git repository, initializing it if needed,
    /// and keep machine-local state out of version control.
    pub fn open_or_init(dir: PathBuf) -> AppResult<Self> {
        if !dir.join(".git").exists() {
            run_git(Some(&dir), &["init", "--quiet"])?;
            // Identity is scoped to this repository only.
            run_git(Some(&dir), &["config", "user.name", "Pinlet"])?;
            run_git(Some(&dir), &["config", "user.email", "pinlet@localhost"])?;
        }
        let repo = Self { dir };
        repo.write_gitignore()?;
        Ok(repo)
    }

    /// Stage every change and commit with `message`.
    /// No-op when the working tree is already clean.
    pub fn commit_all(&self, message: &str) -> AppResult<()> {
        let status = run_git_capture(Some(&self.dir), &["status", "--porcelain"])?;
        if status.trim().is_empty() {
            return Ok(());
        }
        run_git(Some(&self.dir), &["add", "--all"])?;
        run_git(Some(&self.dir), &["commit", "--quiet", "-m", message])?;
        Ok(())
    }

    /// Create `.gitignore` on first run so machine-specific files
    /// never reach version control.
    fn write_gitignore(&self) -> AppResult<()> {
        let path = self.dir.join(".gitignore");
        if path.exists() {
            return Ok(());
        }
        let contents = "# Machine-specific state — never sync\nlocal-state.json\n*.md.tmp\n";
        fs::write(path, contents)?;
        Ok(())
    }
}

/// Run git, mapping non-zero exits to errors.
fn run_git(dir: Option<&Path>, args: &[&str]) -> AppResult<()> {
    run_git_capture(dir, args).map(|_| ())
}

/// Run git and capture its stdout.
fn run_git_capture(dir: Option<&Path>, args: &[&str]) -> AppResult<String> {
    let mut command = Command::new("git");
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    // Never block on credential prompts during a background commit.
    command.env("GIT_TERMINAL_PROMPT", "0");
    let output = command.args(args).output().map_err(AppError::Io)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AppError::Git(stderr.trim().to_owned()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
