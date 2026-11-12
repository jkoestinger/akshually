//! Seam C: what every Forge adapter does the same at the HTTP boundary (where its token comes
//! from, fetching it again after a 401, and how a refused or missing request fails), run against
//! both GitHub and GitLab through `ForgeAdapter`, with a mock server, a fake `gh` or `glab` and a
//! controlled environment. Nothing here reaches the network or real credentials.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use akshually_core::forge::{
    CredentialSources, ForgeAdapter, ForgeError, ForgeKind, GitHub, GitLab, HostConfig, PullRequestRef,
};
use akshually_testkit::TestDirs;
use mockito::{Matcher, Mock, Server, ServerGuard};

/// One Forge, as the shared tests drive it.
struct Forge {
    kind: ForgeKind,
    /// The host the adapter serves.
    host: &'static str,
    /// The Forge's CLI, and the arguments it's asked a token with.
    cli: &'static str,
    cli_args: &'static str,
    /// The environment variables that give `host` a token, the token's last.
    env: &'static [(&'static str, &'static str)],
    /// The Pull Request the tests fetch, the API path it's at, and the fixture answering it.
    pr: (&'static str, u64),
    path: &'static str,
    fixture: &'static str,
    new: fn(HostConfig, CredentialSources) -> Box<dyn ForgeAdapter>,
}

const GITHUB: Forge = Forge {
    kind: ForgeKind::GitHub,
    host: "github.com",
    cli: "gh",
    cli_args: "auth token --hostname github.com",
    env: &[("GITHUB_TOKEN", "from-env")],
    pr: ("acme/web", 482),
    path: "/repos/acme/web/pulls/482",
    fixture: "github/pull.json",
    new: |host, sources| Box::new(GitHub::new(host, sources)),
};

const GITLAB: Forge = Forge {
    kind: ForgeKind::GitLab,
    host: "gitlab.example.com",
    cli: "glab",
    cli_args: "config get token --host gitlab.example.com",
    env: &[("GITLAB_HOST", "gitlab.example.com"), ("GITLAB_TOKEN", "from-env")],
    pr: ("platform/infra", 97),
    path: "/projects/platform%2Finfra/merge_requests/97",
    fixture: "gitlab/mr.json",
    new: |host, sources| Box::new(GitLab::new(host, sources)),
};

/// Runs each shared test against both Forges.
macro_rules! for_both_forges {
    ($($test:ident),* $(,)?) => {
        mod github {
            $(#[test] fn $test() { super::$test(&super::GITHUB) })*
        }
        mod gitlab {
            $(#[test] fn $test() { super::$test(&super::GITLAB) })*
        }
    };
}

for_both_forges!(
    the_token_comes_from_the_forge_s_cli_first,
    when_the_cli_has_no_token_for_the_host_the_environment_is_next,
    the_pasted_token_is_the_last_resort,
    with_no_token_anywhere_nothing_is_sent,
    the_cli_is_asked_once_and_again_after_the_token_is_refused,
    a_refused_token_fails_the_inbox,
    a_missing_pull_request_is_not_found,
);

/// A mock Forge API, and the adapter for `forge` pointed at it.
struct Harness<'f> {
    forge: &'f Forge,
    server: ServerGuard,
    dirs: TestDirs,
}

impl<'f> Harness<'f> {
    fn new(forge: &'f Forge) -> Self {
        Harness { forge, server: Server::new(), dirs: TestDirs::new() }
    }

    /// The adapter, with the token `pasted` into the config, the Forge's CLI logged in to
    /// `cli` (a host and its token; `None` when there's no CLI) and the environment `env`.
    fn adapter(&self, pasted: Option<&str>, cli: Option<(&str, &str)>, env: &[(&str, &str)]) -> Box<dyn ForgeAdapter> {
        let host = HostConfig {
            api_base: Some(self.server.url()),
            token: pasted.map(Into::into),
            ..HostConfig::new(self.forge.kind, self.forge.host)
        };
        let cli = match cli {
            Some((host, token)) => self.fake_cli(host, token),
            None => self.dirs.path().join("no-cli-here"),
        };
        let absent = self.dirs.path().join("not-this-forge-s-cli");
        let (gh, glab) = match self.forge.kind {
            ForgeKind::GitHub => (cli, absent),
            ForgeKind::GitLab => (absent, cli),
        };
        let sources = CredentialSources {
            config: "/home/me/.config/akshually/config.toml".into(),
            gh,
            glab,
            env: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        };
        (self.forge.new)(host, sources)
    }

    /// The adapter with only the Forge's CLI logged in to its host, with `token`.
    fn logged_in(&self, token: &str) -> Box<dyn ForgeAdapter> {
        self.adapter(None, Some((self.forge.host, token)), &[])
    }

    /// The Forge's CLI, printing `token` when asked for `host`'s and failing otherwise, which
    /// counts its runs in `cli-runs`.
    fn fake_cli(&self, host: &str, token: &str) -> PathBuf {
        let cli = self.dirs.path().join(self.forge.cli);
        let runs = self.dirs.path().join("cli-runs");
        let args = self.forge.cli_args.replace(self.forge.host, host);
        let script = format!(
            "#!/bin/sh\n[ \"$1\" = --probe ] && exit 0\necho run >> '{}'\nif [ \"$*\" = '{args}' ]; then echo '{token}'; exit 0; fi\n\
             exit 1\n",
            runs.display()
        );
        std::fs::write(&cli, script).unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A child forked by another test thread while the script was open for writing keeps it
        // "busy" (ETXTBSY) until that child execs; wait for it to be runnable.
        while let Err(e) = std::process::Command::new(&cli).arg("--probe").status() {
            assert_eq!(e.raw_os_error(), Some(26), "can't run the fake {}: {e}", self.forge.cli);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        cli
    }

    fn cli_runs(&self) -> usize {
        std::fs::read_to_string(self.dirs.path().join("cli-runs")).map_or(0, |s| s.lines().count())
    }

    /// `GET` the Pull Request, sent with `token`, answering it.
    fn pull_request(&mut self, token: &str) -> Mock {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(self.forge.fixture);
        let body = std::fs::read_to_string(&fixture).unwrap_or_else(|e| panic!("{}: {e}", fixture.display()));
        self.server
            .mock("GET", self.forge.path)
            .match_header("authorization", format!("Bearer {token}").as_str())
            .with_header("content-type", "application/json")
            .with_body(body)
            .create()
    }

    fn pr_ref(&self) -> PullRequestRef {
        PullRequestRef { host: self.forge.host.into(), repo: self.forge.pr.0.into(), number: self.forge.pr.1 }
    }
}

fn the_token_comes_from_the_forge_s_cli_first(forge: &Forge) {
    let mut h = Harness::new(forge);
    let mock = h.pull_request("from-cli");
    let adapter = h.adapter(Some("pasted"), Some((forge.host, "from-cli")), forge.env);

    adapter.pull_request(&h.pr_ref()).unwrap();

    mock.assert();
}

fn when_the_cli_has_no_token_for_the_host_the_environment_is_next(forge: &Forge) {
    let mut h = Harness::new(forge);
    let mock = h.pull_request("from-env");
    let adapter = h.adapter(Some("pasted"), Some(("forge.other.com", "other-host")), forge.env);

    adapter.pull_request(&h.pr_ref()).unwrap();

    mock.assert();
}

fn the_pasted_token_is_the_last_resort(forge: &Forge) {
    let mut h = Harness::new(forge);
    let mock = h.pull_request("pasted");
    let adapter = h.adapter(Some("pasted"), None, &[]);

    adapter.pull_request(&h.pr_ref()).unwrap();

    mock.assert();
}

fn with_no_token_anywhere_nothing_is_sent(forge: &Forge) {
    let mut h = Harness::new(forge);
    let mock = h.server.mock("GET", Matcher::Any).expect(0).create();
    let adapter = h.adapter(None, None, &[]);

    let error = adapter.pull_request(&h.pr_ref()).unwrap_err();

    mock.assert();
    assert!(matches!(&error, ForgeError::NoCredentials { host, .. } if host == forge.host), "{error:?}");
    let message = error.to_string();
    let login = format!("{} auth login --hostname {}", forge.cli, forge.host);
    assert!(message.contains(&login), "{message}");
    let kind = forge.kind.name().to_lowercase();
    let paste = format!(
        "/home/me/.config/akshually/config.toml:\n\n[[hosts]]\nkind = \"{kind}\"\nhost = \"{}\"\ntoken = \"<your token>\"",
        forge.host
    );
    assert!(message.contains(&paste), "{message}");
}

fn the_cli_is_asked_once_and_again_after_the_token_is_refused(forge: &Forge) {
    let mut h = Harness::new(forge);
    let adapter = h.logged_in("token");
    h.pull_request("token");
    adapter.pull_request(&h.pr_ref()).unwrap();
    adapter.pull_request(&h.pr_ref()).unwrap();
    assert_eq!(h.cli_runs(), 1, "the token wasn't kept between requests");

    h.server.reset();
    h.server.mock("GET", forge.path).with_status(401).with_body(r#"{"message":"401 Unauthorized"}"#).create();
    let error = adapter.pull_request(&h.pr_ref()).unwrap_err();
    assert!(matches!(error, ForgeError::Unauthorized { status: 401, .. }), "{error:?}");
    h.server.reset();
    h.pull_request("token");
    adapter.pull_request(&h.pr_ref()).unwrap();

    assert_eq!(h.cli_runs(), 2, "the refused token wasn't fetched again");
}

fn a_refused_token_fails_the_inbox(forge: &Forge) {
    let mut h = Harness::new(forge);
    let adapter = h.logged_in("token");
    h.server.mock("GET", Matcher::Any).with_status(401).with_body(r#"{"message":"401 Unauthorized"}"#).create();

    let error = adapter.inbox().unwrap_err();

    assert!(matches!(error, ForgeError::Unauthorized { status: 401, .. }), "{error:?}");
}

fn a_missing_pull_request_is_not_found(forge: &Forge) {
    let mut h = Harness::new(forge);
    let adapter = h.logged_in("token");
    h.server.mock("GET", forge.path).with_status(404).with_body(r#"{"message":"404 Not Found"}"#).create();

    let error = adapter.pull_request(&h.pr_ref()).unwrap_err();

    assert!(matches!(&error, ForgeError::NotFound(what) if what == &h.pr_ref().to_string()), "{error:?}");
}
