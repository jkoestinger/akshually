//! Seam C: the GitHub adapter at the HTTP boundary, against a mock server, a fake `gh` and a
//! controlled environment. Nothing here reaches the network or real credentials.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use akshually_core::forge::{
    CredentialSources, ForgeAdapter, ForgeError, ForgeKind, GitHub, HostConfig, PullRequest, PullRequestRef,
    RemoteAnchor, RemoteComment, RemoteThread, adapters,
};
use akshually_core::{BinarySizes, FileChange, Settings, Side};
use akshually_testkit::TestDirs;
use mockito::{Matcher, Mock, Server, ServerGuard};

const TOKEN: &str = "gho_test";

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/github").join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// A `gh` that prints `token` for `auth token --hostname <host>` and fails otherwise, and
/// counts its runs in `<dir>/gh-runs`.
fn fake_gh(dir: &Path, host: &str, token: &str) -> PathBuf {
    let gh = dir.join("gh");
    let runs = dir.join("gh-runs");
    let script = format!(
        "#!/bin/sh\n[ \"$1\" = --probe ] && exit 0\necho run >> '{}'\nif [ \"$*\" = 'auth token --hostname {host}' ]; then echo '{token}'; exit 0; fi\n\
         echo 'no oauth token' >&2\nexit 1\n",
        runs.display()
    );
    std::fs::write(&gh, script).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    // A child forked by another test thread while the script was open for writing keeps it
    // "busy" (ETXTBSY) until that child execs; wait for it to be runnable.
    while let Err(e) = std::process::Command::new(&gh).arg("--probe").status() {
        assert_eq!(e.raw_os_error(), Some(26), "can't run the fake gh: {e}");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    gh
}

fn gh_runs(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("gh-runs")).map_or(0, |s| s.lines().count())
}

/// A mock GitHub API, and the adapter for one host pointed at it.
struct Harness {
    server: ServerGuard,
    dirs: TestDirs,
}

impl Harness {
    fn new() -> Self {
        Harness { server: Server::new(), dirs: TestDirs::new() }
    }

    fn host(&self, host: &str) -> HostConfig {
        HostConfig { api_base: Some(self.server.url()), ..HostConfig::new(ForgeKind::GitHub, host) }
    }

    /// github.com, with `gh` logged in.
    fn github(&self) -> GitHub {
        self.adapter(self.host("github.com"), self.sources(Some(TOKEN), &[]))
    }

    fn sources(&self, gh_token: Option<&str>, env: &[(&str, &str)]) -> CredentialSources {
        let gh = match gh_token {
            Some(token) => fake_gh(self.dirs.path(), "github.com", token),
            None => self.dirs.path().join("no-gh-here"),
        };
        CredentialSources { gh, env: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() }
    }

    fn adapter(&self, host: HostConfig, sources: CredentialSources) -> GitHub {
        GitHub::new(host, sources)
    }

    /// `GET path`, sent with `token` and GitHub's required headers, answering `body`.
    fn get(&mut self, path: &str, token: &str, body: &str) -> Mock {
        self.server
            .mock("GET", path)
            .match_header("authorization", format!("Bearer {token}").as_str())
            .match_header("accept", Matcher::Regex("^application/vnd\\.github".into()))
            .match_header("x-github-api-version", "2022-11-28")
            .match_header("user-agent", Matcher::Regex("^akshually/".into()))
            .with_header("content-type", "application/json")
            .with_body(body)
            .create()
    }

    /// `GET contents/path?ref=sha` for the raw file.
    fn contents(&mut self, path: &str, sha: &str, text: &str) -> Mock {
        self.server
            .mock("GET", format!("/repos/acme/web/contents/{path}").as_str())
            .match_query(Matcher::UrlEncoded("ref".into(), sha.into()))
            .match_header("accept", "application/vnd.github.raw+json")
            .match_header("authorization", format!("Bearer {TOKEN}").as_str())
            .with_body(text)
            .create()
    }
}

fn pr_ref() -> PullRequestRef {
    PullRequestRef { host: "github.com".into(), repo: "acme/web".into(), number: 482 }
}

fn sso_pull_request() -> PullRequest {
    PullRequest {
        id: pr_ref(),
        url: "https://github.com/acme/web/pull/482".into(),
        title: "Add SSO login".into(),
        author: "maria".into(),
        head_branch: "sso".into(),
        head_sha: "head1".into(),
        base_sha: "base1".into(),
    }
}

// --- Recognising Pull Request URLs ---

#[test]
fn recognises_pull_request_urls_on_its_host() {
    let h = Harness::new();
    let github = h.github();

    for url in [
        "https://github.com/acme/web/pull/482",
        "https://github.com/acme/web/pull/482/files",
        "https://github.com/acme/web/pull/482#discussion_r123",
        "https://GitHub.com/acme/web/pull/482?w=1",
    ] {
        assert_eq!(github.recognise(url), Some(pr_ref()), "{url}");
    }
}

#[test]
fn ignores_urls_that_are_not_one_of_its_pull_requests() {
    let h = Harness::new();
    let github = h.github();

    for url in [
        "https://github.com/acme/web/pulls",
        "https://github.com/acme/web/issues/482",
        "https://github.com/acme/web/pull/abc",
        "https://github.com/acme/web",
        "https://github.example.com/acme/web/pull/482",
        "not a url",
    ] {
        assert_eq!(github.recognise(url), None, "{url}");
    }
}

#[test]
fn a_configured_enterprise_host_serves_its_own_urls() {
    let dirs = TestDirs::new();
    let file = dirs.path().join("config.toml");
    std::fs::write(&file, "untracked = false\n\n[[hosts]]\nkind = \"github\"\nhost = \"github.example.com\"\n")
        .unwrap();
    let settings = Settings::load(&file).unwrap();

    let forges = adapters(&settings.hosts, &CredentialSources { gh: "/nonexistent".into(), env: HashMap::new() });

    let url = "https://github.example.com/platform/infra/pull/97";
    let recognised: Vec<_> = forges.iter().filter_map(|f| f.recognise(url)).collect();
    assert_eq!(
        recognised,
        vec![PullRequestRef { host: "github.example.com".into(), repo: "platform/infra".into(), number: 97 }]
    );
    assert!(forges.iter().any(|f| f.recognise("https://github.com/acme/web/pull/482").is_some()), "github.com stays");
}

// --- Pull Request, Changeset, Threads ---

#[test]
fn fetches_the_pull_request() {
    let mut h = Harness::new();
    let mock = h.get("/repos/acme/web/pulls/482", TOKEN, &fixture("pull.json"));

    let pr = h.github().pull_request(&pr_ref()).unwrap();

    mock.assert();
    assert_eq!(pr, sso_pull_request());
}

#[test]
fn fetches_the_changeset_against_the_merge_base_through_every_page_of_files() {
    let mut h = Harness::new();
    h.get("/repos/acme/web/compare/base1...head1", TOKEN, &fixture("compare.json"));
    let page2 = format!("{}/repos/acme/web/pulls/482/files?per_page=100&page=2", h.server.url());
    h.server
        .mock("GET", "/repos/acme/web/pulls/482/files")
        .match_query(Matcher::Exact("per_page=100".into()))
        .with_header("link", &format!("<{page2}>; rel=\"next\", <{page2}>; rel=\"last\""))
        .with_body(fixture("files-1.json"))
        .create();
    h.server
        .mock("GET", "/repos/acme/web/pulls/482/files")
        .match_query(Matcher::Exact("per_page=100&page=2".into()))
        .with_body(fixture("files-2.json"))
        .create();
    let reads = [
        h.contents("src/a.rs", "mb1", "fn a() {}\n"),
        h.contents("src/a.rs", "head1", "fn a2() {}\n"),
        h.contents("docs/new.md", "head1", "# New\n"),
        h.contents("old.txt", "mb1", "bye\n"),
        h.contents("src/b.rs", "mb1", "fn b() {}\n"),
        h.contents("lib/b.rs", "head1", "fn b() {}\n"),
    ];

    let changeset = h.github().changeset(&sso_pull_request()).unwrap();

    reads.iter().for_each(Mock::assert);
    assert_eq!(changeset.base, "mb1");
    let change = FileChange::text;
    assert_eq!(
        changeset.files,
        vec![
            change("docs/new.md", None, Some("# New\n")),
            change("lib/b.rs", None, Some("fn b() {}\n")),
            change("old.txt", Some("bye\n"), None),
            change("src/a.rs", Some("fn a() {}\n"), Some("fn a2() {}\n")),
            change("src/b.rs", Some("fn b() {}\n"), None),
        ]
    );
}

#[test]
fn a_binary_file_is_summarised_by_size() {
    let mut h = Harness::new();
    h.get("/repos/acme/web/compare/base1...head1", TOKEN, &fixture("compare.json"));
    h.server
        .mock("GET", "/repos/acme/web/pulls/482/files")
        .match_query(Matcher::Any)
        .with_body(r#"[{ "filename": "logo.png", "status": "modified", "additions": 0, "deletions": 0 }]"#)
        .create();
    h.server
        .mock("GET", "/repos/acme/web/contents/logo.png")
        .match_query(Matcher::UrlEncoded("ref".into(), "mb1".into()))
        .with_body(b"\x89PNG\r\n\x1a\n\0\0old")
        .create();
    h.server
        .mock("GET", "/repos/acme/web/contents/logo.png")
        .match_query(Matcher::UrlEncoded("ref".into(), "head1".into()))
        .with_body(b"\x89PNG\r\n\x1a\n\0\0newer")
        .create();

    let changeset = h.github().changeset(&sso_pull_request()).unwrap();

    let file = &changeset.files[0];
    assert_eq!(file.binary, Some(BinarySizes { old: Some(13), new: Some(15) }));
    assert_eq!((file.old.as_deref(), file.new.as_deref()), (Some(""), Some("")));
}

#[test]
fn fetches_existing_threads_through_every_page() {
    let mut h = Harness::new();
    let page = |after: serde_json::Value, body: &str, server: &mut ServerGuard| {
        server
            .mock("POST", "/graphql")
            .match_header("authorization", format!("Bearer {TOKEN}").as_str())
            .match_body(Matcher::PartialJson(serde_json::json!({
                "variables": { "owner": "acme", "repo": "web", "number": 482, "after": after }
            })))
            .with_body(body)
            .create()
    };
    let first = page(serde_json::Value::Null, &fixture("threads-1.json"), &mut h.server);
    let second = page("CURSOR1".into(), &fixture("threads-2.json"), &mut h.server);

    let threads = h.github().threads(&sso_pull_request()).unwrap();

    first.assert();
    second.assert();
    let comment = |author: &str, body: &str| RemoteComment { author: author.into(), body: body.into() };
    assert_eq!(
        threads,
        vec![
            RemoteThread {
                id: "PRRT_1".into(),
                anchor: RemoteAnchor::Lines { path: "src/a.rs".into(), side: Side::New, start: 1, end: 1 },
                outdated: false,
                resolved_by: None,
                diff_hunk: "@@ -1 +1 @@\n-fn a() {}\n+fn a2() {}".into(),
                comments: vec![comment("lee", "Why the rename?"), comment("maria", "Clearer.")],
            },
            RemoteThread {
                id: "PRRT_2".into(),
                anchor: RemoteAnchor::Lines { path: "old.txt".into(), side: Side::Old, start: 3, end: 5 },
                outdated: false,
                resolved_by: Some("maria".into()),
                diff_hunk: "@@ -1,5 +0,0 @@".into(),
                comments: vec![comment("ghost", "Still needed?")],
            },
            RemoteThread {
                id: "PRRT_3".into(),
                anchor: RemoteAnchor::Lines { path: "src/a.rs".into(), side: Side::New, start: 6, end: 7 },
                outdated: true,
                resolved_by: None,
                diff_hunk: "@@ -5,3 +5,3 @@\n+fn typo() {}".into(),
                comments: vec![comment("lee", "Typo here.")],
            },
            RemoteThread {
                id: "PRRT_4".into(),
                anchor: RemoteAnchor::File { path: "docs/new.md".into() },
                outdated: false,
                resolved_by: None,
                diff_hunk: String::new(),
                comments: vec![comment("sam", "Link this from the README.")],
            },
        ]
    );
}

#[test]
fn a_missing_pull_request_is_not_found() {
    let mut h = Harness::new();
    h.server.mock("GET", "/repos/acme/web/pulls/482").with_status(404).with_body(r#"{"message":"Not Found"}"#).create();

    let error = h.github().pull_request(&pr_ref()).unwrap_err();

    assert!(matches!(error, ForgeError::NotFound(_)), "{error:?}");
}

#[test]
fn an_api_error_carries_github_s_message() {
    let mut h = Harness::new();
    h.server
        .mock("GET", "/repos/acme/web/pulls/482")
        .with_status(500)
        .with_body(r#"{"message":"Server Error"}"#)
        .create();

    let error = h.github().pull_request(&pr_ref()).unwrap_err();

    assert!(matches!(&error, ForgeError::Api { status: 500, message, .. } if message == "Server Error"), "{error:?}");
}

// --- Credentials: `gh`, then the environment, then the pasted token ---

#[test]
fn the_token_comes_from_gh_first() {
    let mut h = Harness::new();
    let mock = h.get("/repos/acme/web/pulls/482", "from-gh", &fixture("pull.json"));
    let host = HostConfig { token: Some("pasted".into()), ..h.host("github.com") };
    let github = h.adapter(host, h.sources(Some("from-gh"), &[("GITHUB_TOKEN", "from-env")]));

    github.pull_request(&pr_ref()).unwrap();

    mock.assert();
}

#[test]
fn without_gh_the_token_comes_from_github_token() {
    let mut h = Harness::new();
    let mock = h.get("/repos/acme/web/pulls/482", "from-env", &fixture("pull.json"));
    let host = HostConfig { token: Some("pasted".into()), ..h.host("github.com") };
    let github = h.adapter(host, h.sources(None, &[("GITHUB_TOKEN", "from-env")]));

    github.pull_request(&pr_ref()).unwrap();

    mock.assert();
}

#[test]
fn when_gh_has_no_token_for_the_host_the_environment_is_next() {
    let mut h = Harness::new();
    let mock = h.get("/repos/acme/web/pulls/482", "from-env", &fixture("pull.json"));
    // `gh` is logged in to another host only.
    let gh = fake_gh(h.dirs.path(), "github.example.com", "other-host");
    let github = h.adapter(
        h.host("github.com"),
        CredentialSources { gh, env: HashMap::from([("GITHUB_TOKEN".into(), "from-env".into())]) },
    );

    github.pull_request(&pr_ref()).unwrap();

    mock.assert();
}

#[test]
fn the_pasted_token_is_the_last_resort() {
    let mut h = Harness::new();
    let mock = h.get("/repos/acme/web/pulls/482", "pasted", &fixture("pull.json"));
    let host = HostConfig { token: Some("pasted".into()), ..h.host("github.com") };
    let github = h.adapter(host, h.sources(None, &[]));

    github.pull_request(&pr_ref()).unwrap();

    mock.assert();
}

#[test]
fn enterprise_hosts_ask_gh_for_their_host_and_never_get_the_github_com_token() {
    let mut h = Harness::new();
    let ghe = PullRequestRef { host: "github.example.com".into(), ..pr_ref() };
    let mock = h.get("/repos/acme/web/pulls/482", "ghe-from-gh", &fixture("pull.json"));
    let gh = fake_gh(h.dirs.path(), "github.example.com", "ghe-from-gh");
    let env = HashMap::from([("GITHUB_TOKEN".into(), "github-com-token".into())]);
    let github = h.adapter(h.host("github.example.com"), CredentialSources { gh, env });

    github.pull_request(&ghe).unwrap();

    mock.assert();
}

#[test]
fn enterprise_hosts_take_the_enterprise_token_from_the_environment() {
    let mut h = Harness::new();
    let ghe = PullRequestRef { host: "github.example.com".into(), ..pr_ref() };
    let mock = h.get("/repos/acme/web/pulls/482", "ghe-from-env", &fixture("pull.json"));
    let env = [("GITHUB_TOKEN", "github-com-token"), ("GH_ENTERPRISE_TOKEN", "ghe-from-env")];
    let github = h.adapter(h.host("github.example.com"), h.sources(None, &env));

    github.pull_request(&ghe).unwrap();

    mock.assert();
}

#[test]
fn with_no_token_anywhere_nothing_is_sent() {
    let mut h = Harness::new();
    let mock = h.server.mock("GET", Matcher::Any).expect(0).create();
    let env = [("GITHUB_TOKEN", "github-com-token")];
    let github = h.adapter(h.host("github.example.com"), h.sources(None, &env));

    let error = github.pull_request(&PullRequestRef { host: "github.example.com".into(), ..pr_ref() }).unwrap_err();

    mock.assert();
    assert!(matches!(&error, ForgeError::NoCredentials { host } if host == "github.example.com"), "{error:?}");
}

#[test]
fn gh_is_asked_once_and_again_after_the_token_is_refused() {
    let mut h = Harness::new();
    let github = h.github();
    h.get("/repos/acme/web/pulls/482", TOKEN, &fixture("pull.json"));
    github.pull_request(&pr_ref()).unwrap();
    github.pull_request(&pr_ref()).unwrap();
    assert_eq!(gh_runs(h.dirs.path()), 1);

    h.server.reset();
    h.server
        .mock("GET", "/repos/acme/web/pulls/482")
        .with_status(401)
        .with_body(r#"{"message":"Bad credentials"}"#)
        .create();
    let error = github.pull_request(&pr_ref()).unwrap_err();
    assert!(matches!(error, ForgeError::Unauthorized { status: 401, .. }), "{error:?}");
    h.server.reset();
    h.get("/repos/acme/web/pulls/482", TOKEN, &fixture("pull.json"));
    github.pull_request(&pr_ref()).unwrap();

    assert_eq!(gh_runs(h.dirs.path()), 2);
}

// --- New pushes ---

#[test]
fn no_update_while_the_head_is_unchanged() {
    let mut h = Harness::new();
    let mock = h.get("/repos/acme/web/pulls/482", TOKEN, &fixture("pull.json"));

    let update = h.github().update(&sso_pull_request()).unwrap();

    mock.assert();
    assert_eq!(update, None);
}

#[test]
fn a_new_head_is_an_update_counting_the_commits_pushed() {
    let mut h = Harness::new();
    h.get("/repos/acme/web/pulls/482", TOKEN, &fixture("pull-pushed.json"));
    let compare = h.get("/repos/acme/web/compare/head1...head2", TOKEN, &fixture("compare-pushed.json"));

    let update = h.github().update(&sso_pull_request()).unwrap().expect("an update");

    compare.assert();
    assert_eq!(update.pull_request, PullRequest { head_sha: "head2".into(), ..sso_pull_request() });
    assert_eq!(update.commits, Some(2));
}

#[test]
fn a_force_push_is_an_update_without_a_commit_count() {
    let mut h = Harness::new();
    h.get("/repos/acme/web/pulls/482", TOKEN, &fixture("pull-pushed.json"));
    h.server
        .mock("GET", "/repos/acme/web/compare/head1...head2")
        .with_status(404)
        .with_body(r#"{"message":"Not Found"}"#)
        .create();

    let update = h.github().update(&sso_pull_request()).unwrap().expect("an update");

    assert_eq!(update.pull_request.head_sha, "head2");
    assert_eq!(update.commits, None);
}

#[test]
fn polling_again_asks_only_whether_the_pull_request_changed() {
    let mut h = Harness::new();
    let github = h.github();
    let first = h
        .server
        .mock("GET", "/repos/acme/web/pulls/482")
        .match_header("if-none-match", Matcher::Missing)
        .with_header("etag", "\"etag1\"")
        .with_body(fixture("pull-pushed.json"))
        .expect(1)
        .create();
    let compare = h.get("/repos/acme/web/compare/head1...head2", TOKEN, &fixture("compare-pushed.json")).expect(1);
    let unchanged = h
        .server
        .mock("GET", "/repos/acme/web/pulls/482")
        .match_header("if-none-match", "\"etag1\"")
        .match_header("authorization", format!("Bearer {TOKEN}").as_str())
        .with_status(304)
        .expect(1)
        .create();

    let polled = github.update(&sso_pull_request()).unwrap();
    let again = github.update(&sso_pull_request()).unwrap();

    first.assert();
    unchanged.assert();
    compare.assert();
    assert_eq!(again, polled);
    assert_eq!(again.unwrap().commits, Some(2));
}
