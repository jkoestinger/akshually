//! The Forge hosts the human uses, from akshually's config file:
//!
//! ```toml
//! [[hosts]]
//! kind = "github"
//! host = "github.example.com"                     # the web host, as in Pull Request URLs
//! api_base = "https://github.example.com/api/v3"  # optional; derived from `host` by default
//! token = "ghp_…"                                 # optional pasted token, the last resort
//! ```
//!
//! github.com needs no entry; one with `host = "github.com"` overrides the default.

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub hosts: Vec<HostConfig>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    pub kind: ForgeKind,
    /// The web host (`github.com`, `github.example.com`), optionally with a port.
    pub host: String,
    /// The REST API's base URL, when it can't be derived from `host`.
    pub api_base: Option<String>,
    /// A pasted personal access token, used when neither the CLI nor the environment has one.
    pub token: Option<String>,
}

impl HostConfig {
    /// A host of `kind` at `host` with everything else derived.
    pub fn new(kind: ForgeKind, host: impl Into<String>) -> Self {
        HostConfig { kind, host: host.into(), api_base: None, token: None }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub enum ForgeKind {
    #[serde(rename = "github")]
    GitHub,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("can't read {path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("{path} is not a valid akshually config: {source}")]
    Parse { path: PathBuf, source: Box<toml::de::Error> },
}

impl Config {
    /// The config file: `AKSHUALLY_CONFIG`, else `akshually/config.toml` in the platform's
    /// config directory.
    pub fn path() -> PathBuf {
        std::env::var_os("AKSHUALLY_CONFIG").map(PathBuf::from).unwrap_or_else(|| {
            dirs::config_dir().unwrap_or_else(std::env::temp_dir).join("akshually").join("config.toml")
        })
    }

    /// Reads the config at `path`; a missing file is the default config.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(source) => return Err(ConfigError::Read { path: path.into(), source }),
        };
        Config::parse(&text).map_err(|source| ConfigError::Parse { path: path.into(), source: Box::new(source) })
    }

    pub fn parse(text: &str) -> Result<Config, toml::de::Error> {
        toml::from_str(text)
    }

    /// Every host to serve: the configured ones, plus github.com unless configured.
    pub fn hosts(&self) -> Vec<HostConfig> {
        let mut hosts = self.hosts.clone();
        if !hosts.iter().any(|h| h.kind == ForgeKind::GitHub && h.host.eq_ignore_ascii_case("github.com")) {
            hosts.insert(0, HostConfig::new(ForgeKind::GitHub, "github.com"));
        }
        hosts
    }
}
