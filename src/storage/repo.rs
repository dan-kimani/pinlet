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
        }
        // Identity is scoped to this repository only. Set it on every start so
        // a repo initialized elsewhere (e.g. by the installer's postinst) still
        // has a committer identity for auto-commits.
        run_git(Some(&dir), &["config", "user.name", "Pinlet"])?;
        run_git(Some(&dir), &["config", "user.email", "pinlet@localhost"])?;
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

    /// Point `origin` at `url` and rename the current branch to `branch`.
    /// A no-op when `url` is empty. Idempotent, so it can run before every
    /// push/pull.
    pub fn configure_sync(&self, url: &str, branch: &str) -> AppResult<()> {
        if url.is_empty() {
            return Ok(());
        }
        let remotes = run_git_capture(Some(&self.dir), &["remote"])?;
        if remotes.lines().any(|line| line.trim() == "origin") {
            run_git(Some(&self.dir), &["remote", "set-url", "origin", url])?;
        } else {
            run_git(Some(&self.dir), &["remote", "add", "origin", url])?;
        }
        // Rename the current branch to the configured one. This fails on a
        // fresh repo with no commits yet, which is fine — the branch will be
        // created on the first commit.
        let _ = run_git(Some(&self.dir), &["branch", "-M", branch]);
        Ok(())
    }

    /// Pull fast-forward-only changes from `origin`.
    pub fn pull(&self, branch: &str) -> AppResult<()> {
        run_git(Some(&self.dir), &["pull", "--ff-only", "origin", branch])
    }

    /// Push to `origin` and set the upstream branch.
    pub fn push(&self, branch: &str) -> AppResult<()> {
        run_git(Some(&self.dir), &["push", "-u", "origin", branch])
    }

    /// Keep machine-specific and sensitive files out of version
    /// control, merging new entries into an existing `.gitignore`.
    fn write_gitignore(&self) -> AppResult<()> {
        let path = self.dir.join(".gitignore");
        let mut contents = if path.exists() {
            fs::read_to_string(&path)?
        } else {
            "# Machine-specific state — never sync\n".to_owned()
        };
        let mut changed = false;
        for entry in [
            "local-state.json",
            "desktop-positions.json",
            "locked/",
            "*.md.tmp",
        ] {
            if !contents.lines().any(|line| line.trim() == entry) {
                contents.push_str(entry);
                contents.push('\n');
                changed = true;
            }
        }
        if changed {
            fs::write(path, contents)?;
        }
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
