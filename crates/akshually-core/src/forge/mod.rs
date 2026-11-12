//! Forge adapters: how the core reads Pull Requests from GitHub (and later GitLab).
//!
//! The review service holds a set of [`ForgeAdapter`]s, one per configured host, and asks each
//! whether it recognises a Pull Request URL. Adapters return plain core data (a [`Changeset`],
//! [`RemoteThread`]s); the service turns those into a Session.

use serde::{Deserialize, Serialize};

use crate::{BinarySizes, Changeset, FileChange, Side};

mod config;
mod credentials;
mod github;

pub use config::{ForgeKind, HostConfig};
pub use credentials::CredentialSources;
pub use github::GitHub;

/// One Forge host, as the review service sees it. Methods block on the network.
pub trait ForgeAdapter: Send + Sync {
    /// The Pull Request `url` points at, if it's one on this adapter's host.
    fn recognise(&self, url: &str) -> Option<PullRequestRef>;

    /// The Pull Request's current metadata.
    fn pull_request(&self, pr: &PullRequestRef) -> Result<PullRequest, ForgeError>;

    /// The Pull Request's Changeset at `pr.head_sha`, as whole old and new texts per file,
    /// against the merge-base (what the Forge's own diff shows).
    fn changeset(&self, pr: &PullRequest) -> Result<Changeset, ForgeError>;

    /// The Threads already on the Pull Request, from anyone.
    fn threads(&self, pr: &PullRequest) -> Result<Vec<RemoteThread>, ForgeError>;
}

/// Names one Pull Request: a Pull Request Session's identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PullRequestRef {
    /// The Forge's web host, as configured (`github.com`, `github.example.com`).
    pub host: String,
    /// The repository's full path on the host (`owner/repo`; GitLab groups nest).
    pub repo: String,
    pub number: u64,
}

impl std::fmt::Display for PullRequestRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}#{}", self.host, self.repo, self.number)
    }
}

/// A Pull Request's metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    pub id: PullRequestRef,
    /// The Pull Request's web page.
    pub url: String,
    pub title: String,
    /// The author's login.
    pub author: String,
    pub head_branch: String,
    /// The commit the Changeset shows; a new push changes it.
    pub head_sha: String,
    /// The target branch's commit the Pull Request was compared against when fetched.
    pub base_sha: String,
}

/// A Thread already on the Forge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteThread {
    /// The Forge's id for the Thread, for replying and resolving.
    pub id: String,
    pub anchor: RemoteAnchor,
    /// The Forge says the code it was written on has changed since.
    pub outdated: bool,
    pub resolved: bool,
    /// The diff the Thread was written against, as the Forge shows it.
    pub diff_hunk: String,
    /// Oldest first.
    pub comments: Vec<RemoteComment>,
}

/// Where a Thread on the Forge is attached. For an outdated Thread, these are the lines it was
/// written on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteAnchor {
    /// Lines `start..=end` (1-based) of `path` on `side`.
    Lines { path: String, side: Side, start: u32, end: u32 },
    /// The whole file.
    File { path: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteComment {
    /// The writer's login.
    pub author: String,
    /// Markdown.
    pub body: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ForgeError {
    #[error(
        "no credentials for {host}: run `gh auth login --hostname {host}`, set a token in the environment, \
         or paste a token into akshually's config"
    )]
    NoCredentials { host: String },
    #[error("{host} refused the token (HTTP {status}); check it is valid and can read this repository")]
    Unauthorized { host: String, status: u16 },
    #[error("{0} doesn't exist, or the token can't see it")]
    NotFound(String),
    #[error("{host} answered HTTP {status}: {message}")]
    Api { host: String, status: u16, message: String },
    #[error("couldn't reach {host}: {message}")]
    Http { host: String, message: String },
    #[error("unexpected answer from {host}: {message}")]
    Unexpected { host: String, message: String },
}

/// The adapters for github.com and every configured host (`Settings::hosts`).
pub fn adapters(hosts: &[HostConfig], sources: &CredentialSources) -> Vec<Box<dyn ForgeAdapter>> {
    config::with_github_com(hosts)
        .into_iter()
        .map(|host| match host.kind {
            ForgeKind::GitHub => Box::new(GitHub::new(host, sources.clone())) as Box<dyn ForgeAdapter>,
        })
        .collect()
}

/// A changed file from its contents on each side: binary (a NUL in its first 8 KB, as git
/// decides) is summarised by size, text is decoded lossily.
pub(crate) fn file_change(path: String, old: Option<Vec<u8>>, new: Option<Vec<u8>>) -> FileChange {
    let binary = |bytes: &Option<Vec<u8>>| bytes.as_ref().is_some_and(|b| b[..b.len().min(8192)].contains(&0));
    if binary(&old) || binary(&new) {
        let size = |bytes: &Option<Vec<u8>>| bytes.as_ref().map(|b| b.len() as u64);
        let sizes = BinarySizes { old: size(&old), new: size(&new) };
        let (old, new) = (old.map(|_| String::new()), new.map(|_| String::new()));
        return FileChange { path, old, new, binary: Some(sizes), collapsed: None };
    }
    let text = |bytes: Vec<u8>| {
        String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
    };
    FileChange { path, old: old.map(text), new: new.map(text), binary: None, collapsed: None }
}
