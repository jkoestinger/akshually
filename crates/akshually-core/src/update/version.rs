//! Release versions, ordered by semantic-versioning precedence, and which release an install is
//! offered.

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

use super::Release;

/// A release's version, `major.minor.patch` with an optional pre-release part (`0.1.0-rc.1`).
/// Ordered by semantic-versioning precedence: a pre-release comes before its stable release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    /// The dot-separated pre-release identifiers; empty for a stable release.
    pre: Vec<Identifier>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Identifier {
    // Declared first: a numeric identifier sorts before an alphanumeric one.
    Numeric(u64),
    Alphanumeric(String),
}

impl Version {
    /// The version this binary was built as.
    pub fn running() -> Version {
        env!("CARGO_PKG_VERSION").parse().expect("the crate version is a version")
    }

    /// Whether this is a pre-release, such as `0.1.0-rc.1`.
    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }
}

/// Why a string isn't a [`Version`].
#[derive(Debug, thiserror::Error)]
#[error("`{0}` isn't a version like 0.1.0 or 0.1.0-rc.1")]
pub struct VersionError(String);

impl FromStr for Version {
    type Err = VersionError;

    /// Parses `0.1.0`, `0.1.0-rc.1`, or a release tag with its `v` (`v0.1.0`). Build metadata
    /// (`+…`) is ignored, as precedence ignores it.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let bad = || VersionError(text.to_owned());
        let version = text.strip_prefix('v').unwrap_or(text);
        let version = version.split_once('+').map_or(version, |(v, _)| v);
        let (core, pre) = match version.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (version, None),
        };
        let mut numbers = core.split('.').map(|n| n.parse::<u64>().ok());
        let (Some(Some(major)), Some(Some(minor)), Some(Some(patch)), None) =
            (numbers.next(), numbers.next(), numbers.next(), numbers.next())
        else {
            return Err(bad());
        };
        let pre = match pre {
            None => Vec::new(),
            Some(pre) => pre
                .split('.')
                .map(|id| match id.parse() {
                    _ if id.is_empty() => None,
                    Ok(n) => Some(Identifier::Numeric(n)),
                    Err(_) => Some(Identifier::Alphanumeric(id.to_owned())),
                })
                .collect::<Option<_>>()
                .ok_or_else(bad)?,
        };
        Ok(Version { major, minor, patch, pre })
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        let core = (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch));
        core.then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            // Identifier by identifier; when one runs out first, it is the lower.
            (false, false) => self.pre.cmp(&other.pre),
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        for (at, id) in self.pre.iter().enumerate() {
            f.write_str(if at == 0 { "-" } else { "." })?;
            match id {
                Identifier::Numeric(n) => write!(f, "{n}")?,
                Identifier::Alphanumeric(s) => f.write_str(s)?,
            }
        }
        Ok(())
    }
}

/// The release an install at `running` is offered among `releases`: the newest one newer than
/// it, counting pre-releases only when `running` is a pre-release itself. `None` when it is
/// current.
pub fn offered<'a>(running: &Version, releases: &'a [Release]) -> Option<&'a Release> {
    releases
        .iter()
        .filter(|r| r.version > *running && (running.is_prerelease() || !r.version.is_prerelease()))
        .max_by(|a, b| a.version.cmp(&b.version))
}
