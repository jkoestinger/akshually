//! A thin wrapper over the `git` CLI, so akshually gives the same answers as the user's git
//! (worktrees, ignores, attributes) without linking a git library.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Why a `git` command failed.
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
    run(Command::new("git").current_dir(dir), args)
}

/// Runs `git args` in `dir` with its index at `index` instead of the repository's own.
fn git_with_index(dir: &Path, index: &Path, args: &[&str]) -> Result<String, GitError> {
    let bytes = run(Command::new("git").current_dir(dir).env("GIT_INDEX_FILE", index), args)?;
    String::from_utf8(bytes).map_err(|_| GitError::NotUtf8)
}

fn run(cmd: &mut Command, args: &[&str]) -> Result<Vec<u8>, GitError> {
    let out = cmd.args(["-c", "core.quotepath=off", "--no-pager"]).args(args).output()?;
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
    merge_base_of(repo, "HEAD", other)
}

/// The commit where `a` and `b` forked.
pub fn merge_base_of(repo: &Path, a: &str, b: &str) -> Result<String, GitError> {
    Ok(git(repo, &["merge-base", "--end-of-options", a, b])?.trim().to_owned())
}

/// The value of the gitattribute `attr` for each of `paths` (relative to the root of `repo`):
/// `Some(true)` when set, `Some(false)` when unset or `false`, `None` when unspecified.
pub fn bool_attr(repo: &Path, attr: &str, paths: &[&str]) -> Result<HashMap<String, Option<bool>>, GitError> {
    let mut child = Command::new("git")
        .args(["check-attr", "-z", "--stdin", attr])
        .current_dir(repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut input = Vec::new();
    for path in paths {
        input.extend_from_slice(path.as_bytes());
        input.push(0);
    }
    let mut stdin = child.stdin.take().expect("stdin is piped");
    // Written from another thread, so a full stdout pipe can't deadlock us.
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output()?;
    writer.join().expect("the writer doesn't panic")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        return Err(GitError::Failed { args: format!("check-attr {attr}"), stderr });
    }
    // Records are `<path>\0<attr>\0<value>\0`.
    let fields: Vec<&[u8]> = out.stdout.split(|&b| b == 0).collect();
    let mut values = HashMap::new();
    for [path, _, value] in fields.as_chunks::<3>().0 {
        let path = String::from_utf8(path.to_vec()).map_err(|_| GitError::NotUtf8)?;
        let value = match *value {
            b"unspecified" => None,
            b"unset" | b"false" => Some(false),
            _ => Some(true),
        };
        values.insert(path, value);
    }
    Ok(values)
}

/// Untracked, non-ignored files in `repo`, relative to its root.
pub fn untracked(repo: &Path) -> Result<Vec<String>, GitError> {
    let listing = git_bytes(repo, &["ls-files", "--others", "--exclude-standard", "--full-name", "-z", ":/"])?;
    listing
        .split(|&b| b == 0)
        .filter(|f| !f.is_empty())
        .map(|f| String::from_utf8(f.to_vec()).map_err(|_| GitError::NotUtf8))
        .collect()
}

/// Resolves `rev` (a commit-ish, or `<rev>^{tree}`) to an object id.
pub fn rev_parse(repo: &Path, rev: &str) -> Result<String, GitError> {
    Ok(git(repo, &["rev-parse", "--verify", "--end-of-options", rev])?.trim().to_owned())
}

/// Writes the working tree to the object database as a tree, without touching the
/// repository's index: tracked files as they are on disk (staged or not), plus untracked
/// non-ignored files when `untracked` is set. Returns the tree's id.
pub fn working_tree(repo: &Path, untracked: bool) -> Result<String, GitError> {
    let scratch = tempfile::Builder::new().prefix("akshually-index").tempdir()?;
    let index = scratch.path().join("index");
    // Start from a copy of the real index, so git reuses its cached stats instead of hashing
    // every file, and what is staged (new files, removals) carries over.
    let real = PathBuf::from(git(repo, &["rev-parse", "--path-format=absolute", "--git-path", "index"])?.trim());
    match std::fs::File::open(&real) {
        Ok(real) => copy_index(&real, &index)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    git_with_index(repo, &index, &["add", if untracked { "--all" } else { "--update" }, "--", ":/"])?;
    Ok(git_with_index(repo, &index, &["write-tree"])?.trim().to_owned())
}

/// Copies the index `real` to `to`, keeping its mtime. Git trusts an entry whose stat matches
/// the file on disk only when the entry is older than the index file (racy git): a copy with
/// a fresh mtime would make git trust a same-size edit made in the same second as the last
/// index write, and miss it.
fn copy_index(real: &std::fs::File, to: &Path) -> Result<(), GitError> {
    // Both from the one handle, so a concurrent rewrite of the index can't pair one
    // version's contents with another's mtime.
    let mtime = real.metadata()?.modified()?;
    let mut copy = std::fs::File::create(to)?;
    std::io::copy(&mut &*real, &mut copy)?;
    copy.set_modified(mtime)?;
    Ok(())
}

/// The commit HEAD points at.
pub fn head(repo: &Path) -> Result<String, GitError> {
    Ok(git(repo, &["rev-parse", "HEAD"])?.trim().to_owned())
}
