//! Seam C: the GitLab adapter at the HTTP boundary, against a mock server, a fake `glab` and a
//! controlled environment. Nothing here reaches the network or real credentials.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use akshually_core::forge::{
    CredentialSources, ForgeAdapter, ForgeError, ForgeKind, GitLab, HostConfig, InboxItem, InboxRole, PullRequest,
    PullRequestRef, RemoteAnchor, RemoteComment, RemoteThread, Review, adapters,
};
use akshually_core::{Anchor, BinarySizes, FileChange, Party, Service, Settings, Side, Verdict};
use akshually_testkit::TestDirs;
use mockito::{Matcher, Mock, Server, ServerGuard};

const TOKEN: &str = "glpat-test";

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gitlab").join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// A `glab` that prints `token` for `config get token --host <host>` and prints nothing
/// otherwise (as glab does for an unset key).
fn fake_glab(dir: &Path, host: &str, token: &str) -> PathBuf {
    let glab = dir.join("glab");
    let script = format!(
        "#!/bin/sh\n[ \"$1\" = --probe ] && exit 0\nif [ \"$*\" = 'config get token --host {host}' ]; then echo '{token}'; fi\nexit 0\n"
    );
    std::fs::write(&glab, script).unwrap();
    std::fs::set_permissions(&glab, std::fs::Permissions::from_mode(0o755)).unwrap();
    // A child forked by another test thread while the script was open for writing keeps it
    // "busy" (ETXTBSY) until that child execs; wait for it to be runnable.
    while let Err(e) = std::process::Command::new(&glab).arg("--probe").status() {
        assert_eq!(e.raw_os_error(), Some(26), "can't run the fake glab: {e}");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    glab
}

/// A mock GitLab API, and the adapter for one host pointed at it.
struct Harness {
    server: ServerGuard,
    dirs: TestDirs,
}

impl Harness {
    fn new() -> Self {
        Harness { server: Server::new(), dirs: TestDirs::new() }
    }

    fn host(&self, host: &str) -> HostConfig {
        HostConfig { api_base: Some(self.server.url()), ..HostConfig::new(ForgeKind::GitLab, host) }
    }

    fn sources(&self, glab: Option<(&str, &str)>, env: &[(&str, &str)]) -> CredentialSources {
        let glab = match glab {
            Some((host, token)) => fake_glab(self.dirs.path(), host, token),
            None => self.dirs.path().join("no-glab-here"),
        };
        CredentialSources {
            gh: self.dirs.path().join("no-gh-here"),
            glab,
            env: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        }
    }

    /// gitlab.example.com, with `glab` logged in.
    fn gitlab(&self) -> GitLab {
        GitLab::new(self.host("gitlab.example.com"), self.sources(Some(("gitlab.example.com", TOKEN)), &[]))
    }
}

fn mr_ref() -> PullRequestRef {
    PullRequestRef { host: "gitlab.example.com".into(), repo: "platform/infra".into(), number: 97 }
}

// --- Recognising merge request URLs ---

#[test]
fn recognises_merge_request_urls_on_its_host() {
    let h = Harness::new();
    let gitlab = h.gitlab();

    for url in [
        "https://gitlab.example.com/platform/infra/-/merge_requests/97",
        "https://gitlab.example.com/platform/infra/-/merge_requests/97/diffs",
        "https://gitlab.example.com/platform/infra/-/merge_requests/97#note_123",
        "https://GitLab.example.com/platform/infra/-/merge_requests/97?tab=commits",
    ] {
        assert_eq!(gitlab.recognise(url), Some(mr_ref()), "{url}");
    }
    let nested = gitlab.recognise("https://gitlab.example.com/acme/backend/api/-/merge_requests/5");
    assert_eq!(nested, Some(PullRequestRef { repo: "acme/backend/api".into(), number: 5, ..mr_ref() }));
}

#[test]
fn ignores_urls_that_are_not_one_of_its_merge_requests() {
    let h = Harness::new();
    let gitlab = h.gitlab();

    for url in [
        "https://gitlab.example.com/platform/infra/-/merge_requests",
        "https://gitlab.example.com/platform/infra/-/issues/97",
        "https://gitlab.example.com/platform/infra/-/merge_requests/abc",
        "https://gitlab.example.com/-/merge_requests/97",
        "https://gitlab.com/platform/infra/-/merge_requests/97",
        "https://github.com/platform/infra/pull/97",
        "not a url",
    ] {
        assert_eq!(gitlab.recognise(url), None, "{url}");
    }
}

#[test]
fn a_host_under_a_relative_root_drops_the_root_from_project_paths() {
    let h = Harness::new();
    let host = HostConfig {
        api_base: Some("https://example.com/gitlab/api/v4".into()),
        ..HostConfig::new(ForgeKind::GitLab, "example.com")
    };
    let gitlab = GitLab::new(host, h.sources(None, &[]));

    let mr = gitlab.recognise("https://example.com/gitlab/platform/infra/-/merge_requests/97");

    assert_eq!(mr, Some(PullRequestRef { host: "example.com".into(), ..mr_ref() }));
}

#[test]
fn configured_gitlab_hosts_serve_their_own_urls() {
    let dirs = TestDirs::new();
    let file = dirs.path().join("config.toml");
    std::fs::write(&file, "[[hosts]]\nkind = \"gitlab\"\nhost = \"gitlab.example.com\"\n").unwrap();
    let settings = Settings::load(&file).unwrap();
    let sources = CredentialSources { gh: "/nonexistent".into(), glab: "/nonexistent".into(), env: HashMap::new() };

    let forges = adapters(&settings.hosts, &sources);

    let url = "https://gitlab.example.com/platform/infra/-/merge_requests/97";
    let recognised: Vec<_> = forges.iter().filter_map(|f| f.recognise(url)).collect();
    assert_eq!(recognised, vec![mr_ref()]);
    assert!(forges.iter().any(|f| f.recognise("https://github.com/acme/web/pull/482").is_some()), "github.com stays");
}

// --- Merge request, Changeset, Threads ---

impl Harness {
    /// `GET path`, sent with `token` as a Bearer token, answering `body`.
    fn get(&mut self, path: &str, token: &str, body: &str) -> Mock {
        self.server
            .mock("GET", path)
            .match_query(Matcher::Any)
            .match_header("authorization", format!("Bearer {token}").as_str())
            .match_header("user-agent", Matcher::Regex("^akshually/".into()))
            .with_header("content-type", "application/json")
            .with_body(body)
            .create()
    }
}

fn rotate_creds() -> PullRequest {
    PullRequest {
        id: mr_ref(),
        url: "https://gitlab.example.com/platform/infra/-/merge_requests/97".into(),
        title: "Rotate database credentials".into(),
        author: "tom".into(),
        head_branch: "rotate-creds".into(),
        head_sha: "head1".into(),
        base_sha: "mb1".into(),
    }
}

#[test]
fn fetches_the_merge_request_by_its_project_path() {
    let mut h = Harness::new();
    let mock = h.get("/projects/platform%2Finfra/merge_requests/97", TOKEN, &fixture("mr.json"));

    let pr = h.gitlab().pull_request(&mr_ref()).unwrap();

    mock.assert();
    assert_eq!(pr, rotate_creds());
}

impl Harness {
    /// `GET repository/files/path/raw?ref=sha` for the file's bytes.
    fn raw(&mut self, path: &str, sha: &str, bytes: &[u8]) -> Mock {
        let path = path.replace('/', "%2F");
        self.server
            .mock("GET", format!("/projects/platform%2Finfra/repository/files/{path}/raw").as_str())
            .match_query(Matcher::UrlEncoded("ref".into(), sha.into()))
            .match_header("authorization", format!("Bearer {TOKEN}").as_str())
            .with_body(bytes)
            .create()
    }

    /// `GET merge_requests/97/diffs` for `page`, answering `body` and naming `next`.
    fn diffs(&mut self, page: &str, next: &str, body: &str) -> Mock {
        self.server
            .mock("GET", "/projects/platform%2Finfra/merge_requests/97/diffs")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("page".into(), page.into()),
                Matcher::UrlEncoded("per_page".into(), "100".into()),
            ]))
            .match_header("authorization", format!("Bearer {TOKEN}").as_str())
            .with_header("x-next-page", next)
            .with_body(body)
            .create()
    }
}

#[test]
fn fetches_the_changeset_against_the_merge_base_through_every_page_of_diffs() {
    let mut h = Harness::new();
    let pages = [h.diffs("1", "2", &fixture("diffs-1.json")), h.diffs("2", "", &fixture("diffs-2.json"))];
    let reads = [
        h.raw("src/a.rs", "mb1", b"fn a() {}\n"),
        h.raw("src/a.rs", "head1", b"fn a2() {}\n"),
        h.raw("docs/new.md", "head1", b"# New\n"),
        h.raw("old.txt", "mb1", b"bye\n"),
        h.raw("src/b.rs", "mb1", b"fn b() {}\n"),
        h.raw("lib/b.rs", "head1", b"fn b() {}\n"),
    ];

    let changeset = h.gitlab().changeset(&rotate_creds()).unwrap();

    pages.iter().chain(&reads).for_each(Mock::assert);
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
    h.diffs(
        "1",
        "",
        r#"[{ "old_path": "logo.png", "new_path": "logo.png", "new_file": false, "renamed_file": false,
              "deleted_file": false, "diff": "Binary files a/logo.png and b/logo.png differ\n" }]"#,
    );
    h.raw("logo.png", "mb1", b"\x89PNG\r\n\x1a\n\0\0old");
    h.raw("logo.png", "head1", b"\x89PNG\r\n\x1a\n\0\0newer");

    let changeset = h.gitlab().changeset(&rotate_creds()).unwrap();

    let file = &changeset.files[0];
    assert_eq!(file.binary, Some(BinarySizes { old: Some(13), new: Some(15) }));
}

#[test]
fn fetches_existing_diff_threads_through_every_page() {
    let mut h = Harness::new();
    let page = |page: &str, next: &str, body: &str, server: &mut ServerGuard| {
        server
            .mock("GET", "/projects/platform%2Finfra/merge_requests/97/discussions")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("page".into(), page.into()),
                Matcher::UrlEncoded("per_page".into(), "100".into()),
            ]))
            .match_header("authorization", format!("Bearer {TOKEN}").as_str())
            .with_header("x-next-page", next)
            .with_body(body)
            .create()
    };
    let first = page("1", "2", &fixture("discussions-1.json"), &mut h.server);
    let second = page("2", "", &fixture("discussions-2.json"), &mut h.server);

    let threads = h.gitlab().threads(&rotate_creds()).unwrap();

    first.assert();
    second.assert();
    let comment = |author: &str, body: &str| RemoteComment { author: author.into(), body: body.into() };
    // General comments on the merge request aren't diff Threads, and system notes aren't
    // anyone's comments.
    assert_eq!(
        threads,
        vec![
            RemoteThread {
                id: "6a9c1750b37d513a43987b574953fceb50b03ce7".into(),
                anchor: RemoteAnchor::Lines { path: "src/a.rs".into(), side: Side::New, start: 1, end: 1 },
                outdated: false,
                resolved_by: None,
                diff_hunk: String::new(),
                comments: vec![comment("lee", "Why the rename?"), comment("tom", "Clearer.")],
            },
            RemoteThread {
                id: "87805b7c09016a7058e91bdbe7b29d1f284a39e6".into(),
                anchor: RemoteAnchor::Lines { path: "old.txt".into(), side: Side::Old, start: 3, end: 5 },
                outdated: false,
                resolved_by: Some("tom".into()),
                diff_hunk: String::new(),
                comments: vec![comment("ghost", "Still needed?")],
            },
            RemoteThread {
                id: "f1e2d3c4b5a6978877665544332211ffeeddccbb".into(),
                anchor: RemoteAnchor::Lines { path: "src/a.rs".into(), side: Side::New, start: 6, end: 7 },
                outdated: true,
                resolved_by: None,
                diff_hunk: String::new(),
                comments: vec![comment("lee", "Typo here.")],
            },
            RemoteThread {
                id: "aa11bb22cc33dd44ee55ff6677889900aabbccdd".into(),
                anchor: RemoteAnchor::File { path: "docs/new.md".into() },
                outdated: false,
                resolved_by: None,
                diff_hunk: String::new(),
                comments: vec![comment("sam", "Link this from the README.")],
            },
        ]
    );
}

// --- The Inbox ---

impl Harness {
    /// `GET /user`, naming the token's user.
    fn user(&mut self, username: &str) -> Mock {
        self.get("/user", TOKEN, &format!(r#"{{"id": 1, "username": "{username}", "name": "Me"}}"#))
    }

    /// `GET merge_requests` for open ones across every project with `filter` (a query pair).
    fn merge_requests(&mut self, filter: (&str, &str), body: &str) -> Mock {
        self.server
            .mock("GET", "/merge_requests")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("state".into(), "opened".into()),
                Matcher::UrlEncoded("scope".into(), "all".into()),
                Matcher::UrlEncoded(filter.0.into(), filter.1.into()),
                Matcher::UrlEncoded("per_page".into(), "100".into()),
            ]))
            .match_header("authorization", format!("Bearer {TOKEN}").as_str())
            .with_header("content-type", "application/json")
            .with_body(body)
            .create()
    }
}

fn inbox_item(repo: &str, number: u64, title: &str, author: &str, role: InboxRole) -> InboxItem {
    InboxItem {
        id: PullRequestRef { host: "gitlab.example.com".into(), repo: repo.into(), number },
        kind: ForgeKind::GitLab,
        url: format!("https://gitlab.example.com/{repo}/-/merge_requests/{number}"),
        title: title.into(),
        author: author.into(),
        created_at: 0,
        updated_at: 0,
        role,
    }
}

#[test]
fn the_inbox_lists_review_requests_then_assignments_once_each() {
    let mut h = Harness::new();
    let gitlab = h.gitlab();
    h.user("me");
    let requested = h.merge_requests(("reviewer_username", "me"), &fixture("mrs-reviewer.json"));
    let assigned = h.merge_requests(("assignee_username", "me"), &fixture("mrs-assignee.json"));

    let inbox = gitlab.inbox().unwrap();

    requested.assert();
    assigned.assert();
    let listed: Vec<_> = inbox.iter().map(|i| InboxItem { created_at: 0, updated_at: 0, ..i.clone() }).collect();
    assert_eq!(
        listed,
        vec![
            inbox_item("platform/infra", 97, "Rotate database credentials", "tom", InboxRole::Reviewer),
            inbox_item("acme/backend/api", 5, "Add rate limiting", "lee", InboxRole::Reviewer),
            inbox_item("acme/docs", 12, "Fix broken links in setup guide", "sam", InboxRole::Assignee),
        ]
    );
    for item in &inbox {
        assert_eq!(gitlab.recognise(&item.url).as_ref(), Some(&item.id), "{}", item.url);
    }
}

#[test]
fn inbox_times_are_seconds_since_the_epoch() {
    let mut h = Harness::new();
    let gitlab = h.gitlab();
    h.user("me");
    h.merge_requests(("reviewer_username", "me"), &fixture("mrs-reviewer.json"));
    h.merge_requests(("assignee_username", "me"), "[]");

    let inbox = gitlab.inbox().unwrap();

    // 2026-11-11T09:00:00Z and 2026-11-12T08:15:30Z.
    assert_eq!((inbox[0].created_at, inbox[0].updated_at), (1_794_387_600, 1_794_471_330));
    // 2026-11-10T11:00:00Z.
    assert_eq!(inbox[1].created_at, 1_794_308_400);
}

#[test]
fn a_refused_token_fails_the_inbox() {
    let mut h = Harness::new();
    let gitlab = h.gitlab();
    h.server.mock("GET", "/user").with_status(401).with_body(r#"{"message":"401 Unauthorized"}"#).create();

    let error = gitlab.inbox().unwrap_err();

    assert!(matches!(error, ForgeError::Unauthorized { status: 401, .. }), "{error:?}");
}

// --- Credentials: `glab`, then the environment, then the pasted token ---

impl Harness {
    /// The adapter for `host`, its merge request, and a mock expecting `token` for it.
    fn fetch_with(&mut self, host: &str, sources: CredentialSources, pasted: Option<&str>) -> Result<(), ForgeError> {
        let config = HostConfig { token: pasted.map(Into::into), ..self.host(host) };
        GitLab::new(config, sources).pull_request(&PullRequestRef { host: host.into(), ..mr_ref() }).map(drop)
    }
}

#[test]
fn the_token_comes_from_glab_first() {
    let mut h = Harness::new();
    let mock = h.get("/projects/platform%2Finfra/merge_requests/97", "from-glab", &fixture("mr.json"));
    let env = [("GITLAB_HOST", "gitlab.example.com"), ("GITLAB_TOKEN", "from-env")];
    let sources = h.sources(Some(("gitlab.example.com", "from-glab")), &env);

    h.fetch_with("gitlab.example.com", sources, Some("pasted")).unwrap();

    mock.assert();
}

#[test]
fn when_glab_prints_nothing_gitlab_token_serves_the_host_gitlab_host_names() {
    let mut h = Harness::new();
    let mock = h.get("/projects/platform%2Finfra/merge_requests/97", "from-env", &fixture("mr.json"));
    // glab keeps its token in the keyring, or is logged in to another host only.
    let env = [("GITLAB_HOST", "https://gitlab.example.com/"), ("GITLAB_TOKEN", "from-env")];
    let sources = h.sources(Some(("gitlab.other.com", "other-host")), &env);

    h.fetch_with("gitlab.example.com", sources, Some("pasted")).unwrap();

    mock.assert();
}

#[test]
fn without_gitlab_host_gitlab_token_is_gitlab_com_s() {
    let mut h = Harness::new();
    let mock = h.get("/projects/platform%2Finfra/merge_requests/97", "from-env", &fixture("mr.json"));
    let sources = h.sources(None, &[("GITLAB_TOKEN", "from-env")]);

    h.fetch_with("gitlab.com", sources, None).unwrap();

    mock.assert();
}

#[test]
fn gitlab_token_never_goes_to_another_host() {
    let mut h = Harness::new();
    let pasted = h.get("/projects/platform%2Finfra/merge_requests/97", "pasted", &fixture("mr.json"));
    let other = h.server.mock("GET", Matcher::Any).match_header("authorization", "Bearer from-env").expect(0).create();
    let sources = h.sources(None, &[("GITLAB_TOKEN", "from-env")]);

    h.fetch_with("gitlab.example.com", sources, Some("pasted")).unwrap();

    pasted.assert();
    other.assert();
}

#[test]
fn with_no_token_anywhere_nothing_is_sent() {
    let mut h = Harness::new();
    let mock = h.server.mock("GET", Matcher::Any).expect(0).create();
    let sources = h.sources(None, &[("GITLAB_HOST", "gitlab.other.com"), ("GITLAB_TOKEN", "from-env")]);

    let error = h.fetch_with("gitlab.example.com", sources, None).unwrap_err();

    mock.assert();
    assert!(matches!(&error, ForgeError::NoCredentials { host } if host == "gitlab.example.com"), "{error:?}");
}

#[test]
fn opening_a_merge_request_url_starts_a_session_with_its_changeset_and_threads() {
    let mut h = Harness::new();
    h.get("/projects/platform%2Finfra/merge_requests/97", TOKEN, &fixture("mr.json"));
    h.diffs("1", "", &fixture("diffs-1.json"));
    h.raw("src/a.rs", "mb1", b"fn a() {}\n");
    h.raw("src/a.rs", "head1", b"fn a2() {}\n");
    h.raw("docs/new.md", "head1", b"# New\n");
    h.server
        .mock("GET", "/projects/platform%2Finfra/merge_requests/97/discussions")
        .match_query(Matcher::Any)
        .with_body(fixture("discussions-1.json"))
        .create();
    let svc = Service::new().with_forges(vec![Box::new(h.gitlab())]);

    let id = svc.open_pull_request("https://gitlab.example.com/platform/infra/-/merge_requests/97/diffs").unwrap();

    let session = svc.session(&id).expect("the Session is open");
    assert_eq!(session.pull_request, Some(rotate_creds()));
    assert_eq!(session.branch, "rotate-creds");
    let paths: Vec<_> = session.changeset.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["docs/new.md", "src/a.rs"]);
    let thread = &session.threads[0];
    assert_eq!(thread.anchor, Anchor::lines(Side::New, "src/a.rs", 1..=1));
    assert_eq!(thread.excerpt, "fn a2() {}\n");
    assert_eq!(thread.comments[0].author, Party::Other("lee".into()));
}

#[test]
fn publishing_to_gitlab_is_refused_without_sending_anything() {
    let mut h = Harness::new();
    let sent: Vec<Mock> =
        ["GET", "POST", "PUT"].map(|method| h.server.mock(method, Matcher::Any).expect(0).create()).into();
    let gitlab = h.gitlab();
    let review = Review {
        verdict: Verdict::Feedback,
        body: "Looks good.".into(),
        comments: Vec::new(),
        replies: Vec::new(),
        resolve: Vec::new(),
        reopen: Vec::new(),
    };

    let verdicts = gitlab.verdicts(&rotate_creds()).unwrap_err();
    let published = gitlab.publish(&rotate_creds(), &review).unwrap_err();

    sent.iter().for_each(Mock::assert);
    for error in [verdicts, published] {
        assert!(matches!(&error, ForgeError::Unsupported { host, .. } if host == "gitlab.example.com"), "{error:?}");
    }
}

#[test]
fn a_missing_merge_request_is_not_found() {
    let mut h = Harness::new();
    h.server
        .mock("GET", "/projects/platform%2Finfra/merge_requests/97")
        .with_status(404)
        .with_body(r#"{"message":"404 Not found"}"#)
        .create();

    let error = h.gitlab().pull_request(&mr_ref()).unwrap_err();

    assert!(matches!(&error, ForgeError::NotFound(what) if what.contains("platform/infra")), "{error:?}");
}

#[test]
fn an_api_error_carries_gitlab_s_message() {
    let mut h = Harness::new();
    h.server
        .mock("GET", "/projects/platform%2Finfra/merge_requests/97")
        .with_status(500)
        .with_body(r#"{"message":"500 Internal Server Error"}"#)
        .create();

    let error = h.gitlab().pull_request(&mr_ref()).unwrap_err();

    assert!(
        matches!(&error, ForgeError::Api { status: 500, message, .. } if message == "500 Internal Server Error"),
        "{error:?}"
    );
}
