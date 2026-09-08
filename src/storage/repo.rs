//! Git integration for the note repository.
//!
//! v1 shells out to the `git` CLI for auto-commits (zero extra
//! dependencies); Phase 4 swaps the internals for `gix` to add
//! built-in pull/push sync. The public interface stays the same.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::{AppError, AppResult};

/// Wall-clock budget for one git invocation. Local commands finish
/// in milliseconds; network ones get a bounded window instead of
/// hanging the caller — including the main loop — forever on a dead
/// connection.
const GIT_TIMEOUT: Duration = Duration::from_secs(60);

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

    /// Pull and merge from `origin`. Unlike [`GitRepo::pull`], this
    /// will create a merge commit (or leave conflicts in the tree)
    /// rather than refusing a diverged branch.
    pub fn pull_merge(&self, branch: &str) -> AppResult<()> {
        run_git(Some(&self.dir), &["pull", "origin", branch])
    }

    /// Push to `origin` and set the upstream branch.
    pub fn push(&self, branch: &str) -> AppResult<()> {
        run_git(Some(&self.dir), &["push", "-u", "origin", branch])
    }

    /// Whether the index holds unmerged paths — i.e. a merge conflict
    /// is waiting for manual resolution.
    pub fn has_conflicts(&self) -> AppResult<bool> {
        let out = run_git_capture(Some(&self.dir), &["ls-files", "-u"])?;
        Ok(!out.trim().is_empty())
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
        // Appended entries are matched line-wise below: without a
        // trailing newline the first one would glue onto the last line.
        if !contents.is_empty() && !contents.ends_with('\n') {
            contents.push('\n');
        }
        let mut changed = false;
        for entry in [
            "local-state.json",
            "desktop-positions.json",
            "locked/",
            "*.md.tmp",
            // Atomic-write temp files (`settings.json.tmp`,
            // `local-state.json.tmp`) must never be committed if a
            // crash leaves one behind while `git add --all` runs.
            "*.tmp",
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

/// Drain a child pipe on its own thread: a git command filling the
/// pipe buffer must not deadlock the wait loop below.
fn drain_pipe<R: Read + Send + 'static>(
    pipe: Option<R>,
) -> Option<std::thread::JoinHandle<Vec<u8>>> {
    pipe.map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            buf
        })
    })
}

/// Run git and capture its stdout, killing the subprocess if it
/// outlives [`GIT_TIMEOUT`].
fn run_git_capture(dir: Option<&Path>, args: &[&str]) -> AppResult<String> {
    let mut command = Command::new("git");
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    // Never block on credential prompts during a background commit.
    command.env("GIT_TERMINAL_PROMPT", "0");
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.args(args).spawn().map_err(AppError::Io)?;
    let stdout_reader = drain_pipe(child.stdout.take());
    let stderr_reader = drain_pipe(child.stderr.take());
    let deadline = Instant::now() + GIT_TIMEOUT;
    let status = loop {
        match child.try_wait().map_err(AppError::Io)? {
            // try_wait reaps the child, so the status is used from
            // here — a second wait() would fail with ECHILD.
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(AppError::Git("git timed out".to_owned()));
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    let out = stdout_reader
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    let err = stderr_reader
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    if !status.success() {
        let stderr = String::from_utf8_lossy(&err).trim().to_owned();
        // Some failures report on stdout only — never return a blank
        // diagnostic.
        let detail = if stderr.is_empty() {
            String::from_utf8_lossy(&out).trim().to_owned()
        } else {
            stderr
        };
        return Err(AppError::Git(detail));
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}
