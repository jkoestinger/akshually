//! The human's settings, read from akshually's config file.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::forge::HostConfig;

/// The human's settings. Keys missing from the config file keep their defaults.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Whether a local Changeset includes untracked, non-ignored files. A `review` call can
    /// override it.
    pub untracked: bool,
    /// Forge hosts beyond github.com, each with an optional pasted token (`[[hosts]]`).
    pub hosts: Vec<HostConfig>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { untracked: true, hosts: Vec::new() }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("can't read {path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("{path} is not a valid akshually config: {source}")]
    Parse { path: PathBuf, source: Box<toml::de::Error> },
}

impl Settings {
    /// The config file: `AKSHUALLY_CONFIG`, else `config.toml` in the platform's config dir
    /// (`~/.config/akshually` on Linux, `~/Library/Application Support/akshually` on macOS).
    pub fn file_from_env() -> PathBuf {
        std::env::var_os("AKSHUALLY_CONFIG").map(PathBuf::from).unwrap_or_else(|| {
            dirs::config_dir().unwrap_or_else(std::env::temp_dir).join("akshually").join("config.toml")
        })
    }

    /// Reads the settings from the TOML file at `path`; defaults when there is no such file.
    pub fn load(path: &Path) -> Result<Settings, SettingsError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Settings::default()),
            Err(source) => return Err(SettingsError::Read { path: path.to_owned(), source }),
        };
        toml::from_str(&text).map_err(|source| SettingsError::Parse { path: path.to_owned(), source: Box::new(source) })
    }
}
