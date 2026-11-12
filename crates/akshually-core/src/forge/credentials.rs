//! Finding a token for a Forge host, in order: the Forge's CLI (`gh`, `glab`), the environment,
//! then the token pasted into the config. A token meant for one host never goes to another.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use super::{ForgeError, HostConfig};

/// Where tokens come from, injectable so tests use a fake `gh`/`glab` and a controlled
/// environment.
#[derive(Clone, Debug)]
pub struct CredentialSources {
    /// The `gh` program to ask (`gh auth token --hostname <host>`).
    pub gh: PathBuf,
    /// The `glab` program to ask (`glab config get token --host <host>`).
    pub glab: PathBuf,
    /// The environment variables to read tokens from.
    pub env: HashMap<String, String>,
    /// akshually's config file, where the human can paste a token as the last resort.
    pub config: PathBuf,
}

impl CredentialSources {
    /// The `gh` and `glab` on `PATH`, this process's environment and its config file.
    pub fn from_env() -> Self {
        let env =
            std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))).collect();
        CredentialSources { gh: "gh".into(), glab: "glab".into(), env, config: crate::Settings::file_from_env() }
    }

    /// Why there's no token for `host`, telling the human every way to give one.
    pub(crate) fn none_for(&self, host: &HostConfig) -> ForgeError {
        ForgeError::NoCredentials { host: host.host.clone(), kind: host.kind, config: self.config.clone() }
    }

    /// A GitHub token for `host`: `gh`'s, then `GITHUB_TOKEN` for github.com (for GitHub
    /// Enterprise, `GH_ENTERPRISE_TOKEN` or `GITHUB_ENTERPRISE_TOKEN`, as `gh` does, so a
    /// github.com token never goes to another host), then the pasted one.
    pub(crate) fn github_token(&self, host: &HostConfig) -> Option<String> {
        let vars: &[&str] =
            if host.is_default() { &["GITHUB_TOKEN"] } else { &["GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN"] };
        cli_token(Command::new(&self.gh).args(["auth", "token", "--hostname", &host.host]))
            .or_else(|| vars.iter().find_map(|v| non_empty(self.env.get(*v).cloned())))
            .or_else(|| non_empty(host.token.clone()))
    }

    /// A GitLab token for `host`: `glab`'s (it has none to give while it keeps tokens in the OS
    /// keyring), then `GITLAB_TOKEN` if `host` is the one `GITLAB_HOST` names (gitlab.com when
    /// unset), as glab does, then the pasted one.
    pub(crate) fn gitlab_token(&self, host: &HostConfig) -> Option<String> {
        let mut glab = Command::new(&self.glab);
        glab.args(["config", "get", "token", "--host", &host.host]);
        // glab answers with its environment's token whatever the host; ask for its config only.
        for var in ["GITLAB_TOKEN", "GITLAB_ACCESS_TOKEN", "OAUTH_TOKEN"] {
            glab.env_remove(var);
        }
        let env_host = self.env.get("GITLAB_HOST").map_or("gitlab.com", |h| bare_host(h));
        cli_token(&mut glab)
            .or_else(|| {
                let ours = env_host.eq_ignore_ascii_case(&host.host);
                ours.then(|| non_empty(self.env.get("GITLAB_TOKEN").cloned())).flatten()
            })
            .or_else(|| non_empty(host.token.clone()))
    }
}

/// What `command` prints, if it succeeds with some output. A missing program is no token.
fn cli_token(command: &mut Command) -> Option<String> {
    let output = command.stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    if !output.status.success() {
        return None;
    }
    non_empty(String::from_utf8(output.stdout).ok())
}

/// `host` from `GITLAB_HOST`, which glab accepts with or without a scheme.
fn bare_host(value: &str) -> &str {
    let value = value.trim();
    let value = value.split_once("://").map_or(value, |(_, rest)| rest);
    value.trim_end_matches('/')
}

fn non_empty(token: Option<String>) -> Option<String> {
    token.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty())
}
