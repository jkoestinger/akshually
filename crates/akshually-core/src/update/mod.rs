//! Self-update (ADR 0003): which GitHub release an install is offered, and replacing the
//! install the running executable belongs to with it, checked against the release's
//! `SHA256SUMS`.

mod install;
mod releases;
mod version;

pub use install::{Archive, Target, Updater, installed_exe};
pub use releases::Releases;
pub use version::{Version, VersionError, release_offered};

/// One published release of akshually.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    /// The release's tag, such as `v0.1.0`.
    pub tag: String,
    /// The version its tag names.
    pub version: Version,
    /// Every platform's asset, and `SHA256SUMS`.
    pub assets: Vec<Asset>,
}

/// A file attached to a [`Release`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asset {
    /// The file name, such as `akshually-v0.1.0-x86_64-unknown-linux-gnu.tar.gz`.
    pub name: String,
    /// Where it downloads from.
    pub url: String,
}

/// Why looking for, downloading or installing a release failed.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    /// The request to `url` got no answer, or its body couldn't be read.
    #[error("can't reach {url}: {message}")]
    Http { url: String, message: String },
    /// `url` answered with a status other than 2xx; `message` is GitHub's, or the body.
    #[error("{url} answered {status}: {message}")]
    Api { url: String, status: u16, message: String },
    /// The releases list isn't the JSON GitHub sends.
    #[error("GitHub's list of akshually Releases makes no sense: {0}")]
    Unexpected(String),
    /// The release workflow builds nothing for the platform this runs on.
    #[error("there are no akshually Releases for this platform")]
    NoPlatform,
    /// The Release has no asset `name`, this platform's.
    #[error("Release {tag} has no {name} for this platform")]
    NoAsset { tag: String, name: String },
    /// The Release, by tag, has no `SHA256SUMS`.
    #[error("Release {0} has no SHA256SUMS to check its assets against")]
    NoChecksums(String),
    /// `SHA256SUMS` doesn't list this asset.
    #[error("SHA256SUMS doesn't list {0}")]
    NoChecksum(String),
    /// The downloaded asset isn't the one `SHA256SUMS` lists.
    #[error("{0} doesn't match its hash in SHA256SUMS, so nothing was installed; try again later")]
    HashMismatch(String),
    /// The user can't write to the install at `path` (or the directory it is in).
    #[error(
        "can't write to {}, so akshually can't update itself there; install it where it can with the install \
         script: {INSTALL_SCRIPT}",
        path.display()
    )]
    NotWritable { path: std::path::PathBuf },
    /// The asset `name` couldn't be unpacked, or doesn't hold akshually.
    #[error("can't unpack {name}: {message}")]
    Unpack { name: String, message: String },
    /// Writing the Update at `path` failed.
    #[error("can't install to {}: {source}", path.display())]
    Io { path: std::path::PathBuf, source: std::io::Error },
}

/// The install script, which installs where an Update can write (ADR 0003).
const INSTALL_SCRIPT: &str =
    "curl -fsSL https://raw.githubusercontent.com/jkoestinger/akshually/main/install.sh | bash";
