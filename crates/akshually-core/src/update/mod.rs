//! Self-update (ADR 0003): which GitHub release an install is offered, and replacing the
//! install the running executable belongs to with it, checked against the release's
//! `SHA256SUMS`.

mod install;
mod releases;
mod version;

pub use install::{Archive, Target, Updater};

pub use releases::{GITHUB_RELEASES_API, Releases};
pub use version::{Version, VersionError, offered};

/// One published release of akshually.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    /// The release's tag, such as `v0.1.0`.
    pub tag: String,
    pub version: Version,
    pub assets: Vec<Asset>,
}

/// A file attached to a [`Release`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    /// Where it downloads from.
    pub url: String,
}

/// Why looking for, downloading or installing a release failed.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("can't reach {url}: {message}")]
    Http { url: String, message: String },
    #[error("{url} answered {status}: {message}")]
    Api { url: String, status: u16, message: String },
    #[error("GitHub's list of akshually releases makes no sense: {0}")]
    Unexpected(String),
    #[error("there are no akshually releases for this platform")]
    NoPlatform,
    #[error("release {tag} has no {name} for this platform")]
    NoAsset { tag: String, name: String },
    #[error("release {0} has no SHA256SUMS to check its assets against")]
    NoChecksums(String),
    #[error("SHA256SUMS doesn't list {0}")]
    NoChecksum(String),
    #[error("{0} doesn't match its hash in SHA256SUMS, so nothing was installed; try again later")]
    HashMismatch(String),
    #[error(
        "can't write to {}, so akshually can't update itself there; install it where it can with the install \
         script: {script}",
        path.display()
    )]
    NotWritable { path: std::path::PathBuf, script: &'static str },
    #[error("can't unpack {name}: {message}")]
    Unpack { name: String, message: String },
    #[error("can't install to {}: {source}", path.display())]
    Io { path: std::path::PathBuf, source: std::io::Error },
}
