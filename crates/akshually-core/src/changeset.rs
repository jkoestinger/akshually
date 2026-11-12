//! Changesets: the set of changes under Review, as whole old and new texts per file (not
//! patches), so the UI can expand context and later Rounds can diff and re-anchor by content.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::git::{self, GitError};
use crate::{Error, Settings};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changeset {
    /// The commit (or, for a Session base, the tree) the Changeset is shown against.
    pub base: String,
    /// Changed files, sorted by path.
    pub files: Vec<FileChange>,
}

/// One changed file. `old` is `None` for an added file and `new` is `None` for a deleted one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    /// Path relative to the repository root, `/`-separated.
    pub path: String,
    /// The old text; empty for a binary file.
    pub old: Option<String>,
    /// The new text; empty for a binary file.
    pub new: Option<String>,
    /// Set for a binary file, which is summarised rather than diffed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<BinarySizes>,
    /// Set for a file shown collapsed until the human expands it, and why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collapsed: Option<Collapsed>,
}

impl FileChange {
    /// A text file's change, neither binary nor collapsed.
    pub fn text(path: impl Into<String>, old: Option<&str>, new: Option<&str>) -> Self {
        FileChange {
            path: path.into(),
            old: old.map(Into::into),
            new: new.map(Into::into),
            binary: None,
            collapsed: None,
        }
    }
}

/// A binary file's sizes in bytes, on the side(s) where it exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinarySizes {
    pub old: Option<u64>,
    pub new: Option<u64>,
}

/// Why a file is collapsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Collapsed {
    /// Generated: a lockfile, marked `@generated`, or `linguist-generated` in `.gitattributes`.
    Generated,
    /// Too large to show by default.
    Large,
}

/// Which local changes make up a Changeset; every field is optional.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalOptions {
    /// Show the changes since HEAD's merge-base with this ref, instead of the default branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Show exactly a commit range, `<from>..<to>` or `<from>...<to>` (since their
    /// merge-base), instead of the branch and its uncommitted changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<String>,
    /// Include untracked, non-ignored files (`true`) or not, overriding the setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub untracked: Option<bool>,
}

/// A local Source's Changesets: the full one, and the agent's changes since the Session base.
pub(crate) struct LocalChanges {
    pub full: Changeset,
    /// `None` without a Session base, or for a commit range.
    pub since_session_base: Option<Changeset>,
}

/// The local Source's Changesets for `repo`. By default HEAD against its merge-base with the
/// base (the default branch unless `options` name one), plus uncommitted changes (staged or
/// not) and, per the settings or `options`, untracked non-ignored files; with a commit range,
/// exactly that range. `session_base` is a tree from [`snapshot`].
pub(crate) fn local(
    repo: &Path,
    options: &LocalOptions,
    settings: &Settings,
    session_base: Option<&str>,
) -> Result<LocalChanges, Error> {
    if let Some(range) = &options.range {
        if options.base.is_some() {
            return Err(Error::InvalidRequest("give either a base or a commit range, not both".into()));
        }
        let (base, to) = commit_range(repo, range)?;
        let full = diff(repo, base, &format!("{to}^{{tree}}"), &HashSet::new())?;
        return Ok(LocalChanges { full, since_session_base: None });
    }
    let base = match options.base.clone().or_else(|| git::default_branch(repo)) {
        Some(base) => git::merge_base(repo, &base)?,
        None => git::head(repo)?,
    };
    // Untracked files are always in the tree, as in the Session base, and left out of the
    // listing when excluded: otherwise ones that were there at `start` would show as deleted.
    let tree = git::working_tree(repo, true)?;
    let excluded = if options.untracked.unwrap_or(settings.untracked) {
        HashSet::new()
    } else {
        git::untracked(repo)?.into_iter().collect()
    };
    let full = diff(repo, base, &tree, &excluded)?;
    let since_session_base =
        session_base.map(|session_base| diff(repo, session_base.to_owned(), &tree, &excluded)).transpose()?;
    Ok(LocalChanges { full, since_session_base })
}

/// Snapshots the working tree of `repo`, untracked non-ignored files included, as a Session
/// base. Returns the tree's id.
pub(crate) fn snapshot(repo: &Path) -> Result<String, Error> {
    Ok(git::working_tree(repo, true)?)
}

/// Splits `from..to` or `from...to` into the commit to show the range against and `to`.
fn commit_range<'r>(repo: &Path, range: &'r str) -> Result<(String, &'r str), Error> {
    let or_head = |rev: &'r str| if rev.is_empty() { "HEAD" } else { rev };
    if let Some((from, to)) = range.split_once("...") {
        let (from, to) = (or_head(from), or_head(to));
        return Ok((git::merge_base_of(repo, from, to)?, to));
    }
    if let Some((from, to)) = range.split_once("..") {
        let (from, to) = (or_head(from), or_head(to));
        return Ok((git::rev_parse(repo, &format!("{from}^{{commit}}"))?, to));
    }
    Err(Error::InvalidRequest(format!("`{range}` is not a commit range: write `<from>..<to>` or `<from>...<to>`")))
}

/// A file changing more lines than this is collapsed.
const LARGE_LINES: u64 = 2_000;
/// A file larger than this, on either side, is collapsed.
const LARGE_BYTES: usize = 1 << 20;

/// File names that are generated: lockfiles.
const GENERATED_NAMES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lockb",
    "Gemfile.lock",
    "composer.lock",
    "poetry.lock",
    "uv.lock",
    "Pipfile.lock",
    "go.sum",
    "flake.lock",
    "mix.lock",
    "pubspec.lock",
    "Package.resolved",
];

/// The files that differ between the tree-ish `base` and the tree `new`, but for `excluded`.
fn diff(repo: &Path, base: String, new: &str, excluded: &HashSet<String>) -> Result<Changeset, Error> {
    let diff_tree = |format: &str| {
        git::git_bytes(repo, &["diff-tree", "-r", "--no-renames", "--ignore-submodules", format, "-z", &base, new])
    };
    let listing = diff_tree("--name-status")?;
    let changed_lines = changed_lines(&diff_tree("--numstat")?)?;
    let mut fields = listing.split(|&b| b == 0).filter(|f| !f.is_empty());
    let mut changes = Vec::new();
    while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
        let path = String::from_utf8(path.to_vec()).map_err(|_| GitError::NotUtf8)?;
        if !excluded.contains(&path) {
            changes.push((status != b"A", status != b"D", path));
        }
    }
    let paths: Vec<&str> = changes.iter().map(|(_, _, path)| path.as_str()).collect();
    let generated_attr = git::bool_attr(repo, "linguist-generated", &paths)?;

    let mut files = Vec::new();
    for (in_old, in_new, path) in changes {
        let object = |tree: &str| format!("{tree}:{path}");
        // git reports no line counts for a binary file (by content or `.gitattributes`).
        let Some(lines) = changed_lines.get(&path).copied().flatten() else {
            let size = |tree: &str| -> Result<u64, Error> {
                let size = git::git(repo, &["cat-file", "-s", &object(tree)])?;
                Ok(size.trim().parse().map_err(|_| GitError::NotUtf8)?)
            };
            let sizes = BinarySizes {
                old: in_old.then(|| size(&base)).transpose()?,
                new: in_new.then(|| size(new)).transpose()?,
            };
            let (old, new) = (in_old.then(String::new), in_new.then(String::new));
            files.push(FileChange { path, old, new, binary: Some(sizes), collapsed: None });
            continue;
        };
        let blob = |tree: &str| git::git_bytes(repo, &["cat-file", "blob", &object(tree)]).map(text);
        let old = in_old.then(|| blob(&base)).transpose()?;
        let new = in_new.then(|| blob(new)).transpose()?;
        let generated = generated_attr.get(&path).copied().flatten().unwrap_or_else(|| {
            let name = path.rsplit('/').next().unwrap_or(&path);
            GENERATED_NAMES.contains(&name) || new.as_deref().or(old.as_deref()).is_some_and(marked_generated)
        });
        let large =
            lines > LARGE_LINES || [&old, &new].iter().any(|t| t.as_ref().is_some_and(|t| t.len() > LARGE_BYTES));
        let collapsed = if generated {
            Some(Collapsed::Generated)
        } else if large {
            Some(Collapsed::Large)
        } else {
            None
        };
        files.push(FileChange { path, old, new, binary: None, collapsed });
    }
    Ok(Changeset { base, files })
}

/// Lines added plus deleted per path, from `diff-tree --numstat -z`; `None` for a binary file.
fn changed_lines(numstat: &[u8]) -> Result<HashMap<String, Option<u64>>, Error> {
    let mut lines = HashMap::new();
    // Each record is `<added>\t<deleted>\t<path>`, with `-` for both counts on a binary file.
    for record in numstat.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        let mut parts = record.splitn(3, |&b| b == b'\t');
        let (Some(added), Some(deleted), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let count = |n: &[u8]| std::str::from_utf8(n).ok()?.parse::<u64>().ok();
        let path = String::from_utf8(path.to_vec()).map_err(|_| GitError::NotUtf8)?;
        lines.insert(path, count(added).zip(count(deleted)).map(|(a, d)| a + d));
    }
    Ok(lines)
}

/// Whether `text` says it is generated, by an `@generated` marker in its first lines.
fn marked_generated(text: &str) -> bool {
    text.lines().take(5).any(|line| line.contains("@generated"))
}

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}
