//! A thin wrapper over the `git` CLI, so akshually gives the same answers as the user's git
//! (worktrees, ignores, attributes) without linking a git library.

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("could not run git: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("`git {args}` failed: {stderr}")]
    Failed { args: String, stderr: String },
    #[error("git printed something that isn't UTF-8")]
    NotUtf8,
}

/// Runs `git args` in `dir` and returns its stdout.
pub fn git(dir: &Path, args: &[&str]) -> Result<String, GitError> {
    let bytes = git_bytes(dir, args)?;
    String::from_utf8(bytes).map_err(|_| GitError::NotUtf8)
}

/// Runs `git args` in `dir` and returns its raw stdout.
pub fn git_bytes(dir: &Path, args: &[&str]) -> Result<Vec<u8>, GitError> {
    let out =
        Command::new("git").args(["-c", "core.quotepath=off", "--no-pager"]).args(args).current_dir(dir).output()?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(GitError::Failed { args: args.join(" "), stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned() })
    }
}

/// Runs `git args` in `dir`, mapping a failure to `None` (for questions like "does this ref
/// exist?").
fn git_ok(dir: &Path, args: &[&str]) -> Option<String> {
    git(dir, args).ok().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

/// The root of the working tree containing `dir` (each worktree is its own root).
pub fn toplevel(dir: &Path) -> Result<PathBuf, GitError> {
    Ok(PathBuf::from(git(dir, &["rev-parse", "--show-toplevel"])?.trim()))
}

/// The checked-out branch, or `HEAD@<sha>` when HEAD is detached.
pub fn branch(repo: &Path) -> Result<String, GitError> {
    if let Some(branch) = git_ok(repo, &["symbolic-ref", "--short", "-q", "HEAD"]) {
        return Ok(branch);
    }
    Ok(format!("HEAD@{}", git(repo, &["rev-parse", "--short", "HEAD"])?.trim()))
}

/// The repository's default branch: `origin/HEAD` when known, else `init.defaultBranch`,
/// `main` or `master`, whichever exists first. `None` when there is none of those.
pub fn default_branch(repo: &Path) -> Option<String> {
    if let Some(remote_head) = git_ok(repo, &["symbolic-ref", "--short", "-q", "refs/remotes/origin/HEAD"]) {
        return Some(remote_head);
    }
    let configured = git_ok(repo, &["config", "init.defaultBranch"]);
    configured
        .into_iter()
        .chain(["main".to_owned(), "master".to_owned()])
        .find(|b| git_ok(repo, &["rev-parse", "--verify", "-q", &format!("refs/heads/{b}")]).is_some())
}

/// The commit where HEAD forked from `other`.
pub fn merge_base(repo: &Path, other: &str) -> Result<String, GitError> {
    Ok(git(repo, &["merge-base", "HEAD", other])?.trim().to_owned())
}

/// The commit HEAD points at.
pub fn head(repo: &Path) -> Result<String, GitError> {
    Ok(git(repo, &["rev-parse", "HEAD"])?.trim().to_owned())
}
