//! The Forge hosts the human uses, as `[[hosts]]` in akshually's config file (see
//! [`Settings`](crate::Settings)):
//!
//! ```toml
//! [[hosts]]
//! kind = "github"
//! host = "github.example.com"                     # the web host, as in Pull Request URLs
//! api_base = "https://github.example.com/api/v3"  # optional; derived from `host` by default
//! token = "ghp_…"                                 # optional pasted token, the last resort
//!
//! [[hosts]]
//! kind = "gitlab"
//! host = "gitlab.example.com"
//! api_base = "https://gitlab.example.com/api/v4"  # derived by default; set it for a relative root
//! ```
//!
//! github.com needs no entry; one with `host = "github.com"` overrides the default. GitLab hosts,
//! gitlab.com included, are served only when configured.

use serde::{Deserialize, Serialize};

/// One Forge host, as a `[[hosts]]` entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    pub kind: ForgeKind,
    /// The web host (`github.com`, `gitlab.example.com`), optionally with a port.
    pub host: String,
    /// The REST API's base URL, when it can't be derived from `host`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base: Option<String>,
    /// A pasted personal access token, used when neither the CLI nor the environment has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

impl HostConfig {
    /// A host of `kind` at `host` with everything else derived.
    pub fn new(kind: ForgeKind, host: impl Into<String>) -> Self {
        HostConfig { kind, host: host.into(), api_base: None, token: None }
    }

    /// Whether this is its Forge's own public host, github.com or gitlab.com.
    pub fn is_default(&self) -> bool {
        self.kind.is_default_host(&self.host)
    }
}

/// Which Forge a host runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ForgeKind {
    #[serde(rename = "github")]
    GitHub,
    #[serde(rename = "gitlab")]
    GitLab,
}

impl ForgeKind {
    /// What precedes a Pull Request's number on this Forge: GitLab numbers merge requests `!97`.
    pub fn sigil(self) -> char {
        match self {
            ForgeKind::GitHub => '#',
            ForgeKind::GitLab => '!',
        }
    }

    /// The Forge's name, as the human reads it.
    pub fn name(self) -> &'static str {
        match self {
            ForgeKind::GitHub => "GitHub",
            ForgeKind::GitLab => "GitLab",
        }
    }

    /// The Forge's own public host.
    pub fn default_host(self) -> &'static str {
        match self {
            ForgeKind::GitHub => "github.com",
            ForgeKind::GitLab => "gitlab.com",
        }
    }

    /// Whether `host` is the Forge's own public host, whatever its case.
    pub fn is_default_host(self, host: &str) -> bool {
        host.eq_ignore_ascii_case(self.default_host())
    }
}

/// Every host to serve: `configured`, plus github.com unless configured.
pub(super) fn with_github_com(configured: &[HostConfig]) -> Vec<HostConfig> {
    let mut hosts = configured.to_vec();
    if !hosts.iter().any(|h| h.kind == ForgeKind::GitHub && h.is_default()) {
        hosts.insert(0, HostConfig::new(ForgeKind::GitHub, ForgeKind::GitHub.default_host()));
    }
    hosts
}
