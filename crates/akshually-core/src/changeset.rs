//! Changesets: the set of changes under Review, as whole old and new texts per file (not
//! patches), so the UI can expand context and later Rounds can diff and re-anchor by content.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::Error;
use crate::git::{self, GitError};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changeset {
    /// The commit the Changeset is shown against.
    pub base: String,
    /// Changed files, sorted by path.
    pub files: Vec<FileChange>,
}

/// One changed file. `old` is `None` for an added file and `new` is `None` for a deleted one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    /// Path relative to the repository root, `/`-separated.
    pub path: String,
    pub old: Option<String>,
    pub new: Option<String>,
}

/// The local Source's Changeset for `repo`: HEAD against its merge-base with the default
/// branch, plus uncommitted changes (staged or not) to tracked files.
pub(crate) fn local(repo: &Path) -> Result<Changeset, Error> {
    let base = match git::default_branch(repo) {
        Some(default) => git::merge_base(repo, &default)?,
        None => git::head(repo)?,
    };
    // `git diff <commit>` compares the working tree, index included, with the commit.
    let listing = git::git_bytes(repo, &["diff", "--no-ext-diff", "--no-renames", "--name-status", "-z", &base])?;
    let mut fields = listing.split(|&b| b == 0).filter(|f| !f.is_empty());
    let mut files = Vec::new();
    while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
        let path = String::from_utf8(path.to_vec()).map_err(|_| GitError::NotUtf8)?;
        let old = match status {
            b"A" => None,
            _ => Some(text(git::git_bytes(repo, &["cat-file", "blob", &format!("{base}:{path}")])?)),
        };
        let new = match status {
            b"D" => None,
            _ => Some(text(std::fs::read(repo.join(&path))?)),
        };
        files.push(FileChange { path, old, new });
    }
    Ok(Changeset { base, files })
}

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}
