//! Finding a token for a Forge host, in order: the Forge's CLI (`gh`), the environment, then
//! the token pasted into the config.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use super::HostConfig;

/// Where tokens come from, injectable so tests use a fake `gh` and a controlled environment.
#[derive(Clone, Debug)]
pub struct CredentialSources {
    /// The `gh` program to ask (`gh auth token --hostname <host>`).
    pub gh: PathBuf,
    /// The environment variables to read tokens from.
    pub env: HashMap<String, String>,
}

impl CredentialSources {
    /// The `gh` on `PATH` and this process's environment.
    pub fn from_env() -> Self {
        let env =
            std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))).collect();
        CredentialSources { gh: "gh".into(), env }
    }

    /// A GitHub token for `host`: `gh`'s, then `GITHUB_TOKEN` for github.com (for GitHub
    /// Enterprise, `GH_ENTERPRISE_TOKEN` or `GITHUB_ENTERPRISE_TOKEN`, as `gh` does, so a
    /// github.com token never goes to another host), then the pasted one.
    pub(crate) fn github_token(&self, host: &HostConfig) -> Option<String> {
        let vars: &[&str] = if host.host.eq_ignore_ascii_case("github.com") {
            &["GITHUB_TOKEN"]
        } else {
            &["GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN"]
        };
        self.cli_token(&["auth", "token", "--hostname", &host.host])
            .or_else(|| vars.iter().find_map(|v| non_empty(self.env.get(*v).cloned())))
            .or_else(|| non_empty(host.token.clone()))
    }

    /// What `gh args` prints, if it succeeds with some output. A missing `gh` is no token.
    fn cli_token(&self, args: &[&str]) -> Option<String> {
        let output = Command::new(&self.gh).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
        if !output.status.success() {
            return None;
        }
        non_empty(String::from_utf8(output.stdout).ok())
    }
}

fn non_empty(token: Option<String>) -> Option<String> {
    token.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty())
}
