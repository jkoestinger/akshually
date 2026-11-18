//! Seam C: the GitHub adapter at the HTTP boundary, against a mock server, a fake `gh` and a
//! controlled environment: GitHub's endpoints, error body and credentials; what every adapter does
//! the same is in `forge_http.rs`. Nothing here reaches the network or real credentials.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use akshually_core::forge::{
    CredentialSources, ForgeAdapter, ForgeError, ForgeKind, GitHub, HostConfig, InboxItem, InboxRole, PullRequest,
    PullRequestRef, RemoteAnchor, RemoteComment, RemoteThread, adapters,
};
use akshually_core::{BinarySizes, FileChange, Hunk, Settings, Side};
use akshually_testkit::{TestDirs, fake_cli, threaded_http_server};
use mockito::{Matcher, Mock, Server, ServerGuard};

const TOKEN: &str = "gho_test";

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/github").join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// A `gh` that prints `token` for `auth token --hostname <host>` and fails otherwise.
fn fake_gh(dir: &Path, host: &str, token: &str) -> PathBuf {
    let body = format!(
        "if [ \"$*\" = 'auth token --hostname {host}' ]; then echo '{token}'; exit 0; fi
echo 'no oauth token' >&2
exit 1
"
    );
    fake_cli(&dir.join("gh"), &body)
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
        CredentialSources {
            config: "/home/me/.config/akshually/config.toml".into(),
            gh,
            glab: "/nonexistent".into(),
            env: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        }
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

/// No `gh`, no `glab` and an empty environment: no credentials for any host but pasted ones.
fn no_credentials() -> CredentialSources {
    CredentialSources {
        config: "/home/me/.config/akshually/config.toml".into(),
        gh: "/nonexistent".into(),
        glab: "/nonexistent".into(),
        env: HashMap::new(),
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

    let forges = adapters(&settings.hosts, &no_credentials());

    let url = "https://github.example.com/platform/infra/pull/97";
    let recognised: Vec<_> = forges.iter().filter_map(|f| f.recognise(url)).collect();
    assert_eq!(
        recognised,
        vec![PullRequestRef { host: "github.example.com".into(), repo: "platform/infra".into(), number: 97 }]
    );
    assert!(forges.iter().any(|f| f.recognise("https://github.com/acme/web/pull/482").is_some()), "github.com stays");
}

#[test]
fn turning_github_com_off_removes_it() {
    let dirs = TestDirs::new();
    let file = dirs.path().join("config.toml");
    std::fs::write(
        &file,
        "[[hosts]]\nkind = \"github\"\nhost = \"github.com\"\nenabled = false\n\n\
         [[hosts]]\nkind = \"github\"\nhost = \"github.example.com\"\n",
    )
    .unwrap();
    let settings = Settings::load(&file).unwrap();

    let forges = adapters(&settings.hosts, &no_credentials());

    assert!(forges.iter().all(|f| f.recognise("https://github.com/acme/web/pull/482").is_none()), "github.com stays");
    assert_eq!(forges.len(), 1, "only github.example.com is left");
}

#[test]
fn a_config_without_enabled_turns_every_host_on() {
    let dirs = TestDirs::new();
    let file = dirs.path().join("config.toml");
    std::fs::write(&file, "[[hosts]]\nkind = \"github\"\nhost = \"github.example.com\"\ntoken = \"ghp_pasted\"\n")
        .unwrap();

    let settings = Settings::load(&file).unwrap();

    assert_eq!(
        settings.hosts,
        vec![HostConfig {
            token: Some("ghp_pasted".into()),
            ..HostConfig::new(ForgeKind::GitHub, "github.example.com")
        }]
    );
    assert!(settings.hosts[0].enabled);
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
    // Line comments only fit GitHub's own diff: its hunks, and none on a rename's lines.
    let change = |path, old, new, hunks: &[(u32, u32, u32, u32)]| {
        let hunks = hunks.iter().map(|&(old_start, old_lines, new_start, new_lines)| Hunk {
            old_start,
            old_lines,
            new_start,
            new_lines,
        });
        FileChange { hunks: Some(hunks.collect()), ..FileChange::text(path, old, new) }
    };
    assert_eq!(
        changeset.files,
        vec![
            change("docs/new.md", None, Some("# New\n"), &[(0, 0, 1, 1)]),
            change("lib/b.rs", None, Some("fn b() {}\n"), &[]),
            change("old.txt", Some("bye\n"), None, &[(1, 1, 0, 0)]),
            change("src/a.rs", Some("fn a() {}\n"), Some("fn a2() {}\n"), &[(1, 1, 1, 1)]),
            change("src/b.rs", Some("fn b() {}\n"), None, &[]),
        ]
    );
}

#[test]
fn file_contents_are_fetched_in_parallel_and_kept_in_path_order() {
    let paths: Vec<String> = (0..16).map(|n| format!("src/f{n:02}.rs")).collect();
    let listed: Vec<String> =
        paths.iter().rev().map(|p| format!(r#"{{ "filename": "{p}", "status": "modified" }}"#)).collect();
    let listed = format!("[{}]", listed.join(","));
    let api = threaded_http_server(move |target| {
        if target.starts_with("/repos/acme/web/compare/") {
            fixture("compare.json")
        } else if target.starts_with("/repos/acme/web/pulls/482/files") {
            listed.clone()
        } else {
            // Each read takes 100 ms: 32 of them one after the other would take 3.2 s.
            std::thread::sleep(Duration::from_millis(100));
            format!("{target}\n")
        }
    });
    let h = Harness::new();
    let host = HostConfig { api_base: Some(api), ..HostConfig::new(ForgeKind::GitHub, "github.com") };
    let github = h.adapter(host, h.sources(Some(TOKEN), &[]));

    let started = Instant::now();
    let changeset = github.changeset(&sso_pull_request()).unwrap();

    assert!(started.elapsed() < Duration::from_millis(1600), "took {:?}", started.elapsed());
    assert_eq!(changeset.files.iter().map(|f| f.path.clone()).collect::<Vec<_>>(), paths);
    let first = &changeset.files[0];
    assert_eq!(first.old.as_deref(), Some("/repos/acme/web/contents/src/f00.rs?ref=mb1\n"));
    assert_eq!(first.new.as_deref(), Some("/repos/acme/web/contents/src/f00.rs?ref=head1\n"));
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
    assert_eq!(file.hunks, None, "GitHub gave no patch to take hunks from");
}

/// Mocks the GraphQL query for the page of Threads after `after`, with `body` as its answer.
fn threads_page(server: &mut ServerGuard, after: serde_json::Value, body: &str) -> Mock {
    server
        .mock("POST", "/graphql")
        .match_header("authorization", format!("Bearer {TOKEN}").as_str())
        .match_body(Matcher::PartialJson(serde_json::json!({
            "variables": { "owner": "acme", "repo": "web", "number": 482, "after": after }
        })))
        .with_body(body)
        .create()
}

/// Mocks the GraphQL query for Thread `id`'s Comments after `after`, with `body` as its answer.
fn comments_page(server: &mut ServerGuard, id: &str, after: &str, body: &str) -> Mock {
    server
        .mock("POST", "/graphql")
        .match_header("authorization", format!("Bearer {TOKEN}").as_str())
        .match_body(Matcher::PartialJson(serde_json::json!({ "variables": { "id": id, "after": after } })))
        .with_body(body)
        .create()
}

#[test]
fn fetches_existing_threads_through_every_page() {
    let mut h = Harness::new();
    let first = threads_page(&mut h.server, serde_json::Value::Null, &fixture("threads-1.json"));
    let second = threads_page(&mut h.server, "CURSOR1".into(), &fixture("threads-2.json"));

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
fn a_thread_s_comments_are_fetched_through_every_page() {
    let mut h = Harness::new();
    let first = threads_page(&mut h.server, serde_json::Value::Null, &fixture("thread-comments-1.json"));
    let second = comments_page(&mut h.server, "PRRT_9", "COMMENTS1", &fixture("thread-comments-2.json"));
    let third = comments_page(&mut h.server, "PRRT_9", "COMMENTS2", &fixture("thread-comments-3.json"));

    let threads = h.github().threads(&sso_pull_request()).unwrap();

    first.assert();
    second.assert();
    third.assert();
    let comment = |author: &str, body: &str| RemoteComment { author: author.into(), body: body.into() };
    assert_eq!(
        threads,
        vec![RemoteThread {
            id: "PRRT_9".into(),
            anchor: RemoteAnchor::Lines { path: "src/a.rs".into(), side: Side::New, start: 1, end: 1 },
            outdated: false,
            resolved_by: None,
            diff_hunk: "@@ -1 +1 @@\n-fn a() {}\n+fn a2() {}".into(),
            comments: vec![
                comment("lee", "Why the rename?"),
                comment("maria", "Clearer."),
                comment("lee", "Fair enough.")
            ],
        }]
    );
}

#[test]
fn a_failed_query_for_a_thread_s_further_comments_fails_the_threads() {
    let mut h = Harness::new();
    threads_page(&mut h.server, serde_json::Value::Null, &fixture("thread-comments-1.json"));
    let failed = r#"{"data":null,"errors":[{"message":"Something went wrong while executing your query."}]}"#;
    comments_page(&mut h.server, "PRRT_9", "COMMENTS1", failed);

    let result = h.github().threads(&sso_pull_request());

    assert!(
        matches!(&result, Err(ForgeError::Api { message, .. }) if message.contains("Something went wrong")),
        "expected the follow-up's error, got {result:?}"
    );
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
fn enterprise_hosts_ask_gh_for_their_own_host() {
    let mut h = Harness::new();
    let ghe = PullRequestRef { host: "github.example.com".into(), ..pr_ref() };
    let mock = h.get("/repos/acme/web/pulls/482", "ghe-from-gh", &fixture("pull.json"));
    let gh = fake_gh(h.dirs.path(), "github.example.com", "ghe-from-gh");
    let github = h.adapter(
        h.host("github.example.com"),
        CredentialSources {
            config: "/home/me/.config/akshually/config.toml".into(),
            gh,
            glab: "/nonexistent".into(),
            env: HashMap::new(),
        },
    );

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
    assert!(matches!(&error, ForgeError::NoCredentials { host, .. } if host == "github.example.com"), "{error:?}");
    let message = error.to_string();
    assert!(message.contains("gh auth login --hostname github.example.com"), "{message}");
    assert!(message.contains("GH_ENTERPRISE_TOKEN"), "{message}");
    let paste = "/home/me/.config/akshually/config.toml:\n\n[[hosts]]\nkind = \"github\"\n\
                 host = \"github.example.com\"\ntoken = \"<your token>\"";
    assert!(message.contains(paste), "{message}");
}

// --- The Inbox ---

impl Harness {
    /// The GraphQL search for the open Pull Requests matching `filter`, answering `body`.
    fn search(&mut self, filter: &str, body: &str) -> Mock {
        let q = format!("is:pr is:open archived:false sort:updated-desc {filter}");
        self.server
            .mock("POST", "/graphql")
            .match_header("authorization", format!("Bearer {TOKEN}").as_str())
            .match_body(Matcher::PartialJson(serde_json::json!({ "variables": { "q": q } })))
            .with_header("content-type", "application/json")
            .with_body(body)
            .create()
    }

    /// The Inbox of a human asked to review `acme/web#482` alone, which is `as_found` (fields of
    /// the GraphQL search's Pull Request node).
    fn inbox_of_one_review_request(&mut self, as_found: serde_json::Value) -> Vec<InboxItem> {
        self.only_found("review-requested:@me", as_found);
        self.search("assignee:@me", NONE_FOUND);
        self.github().inbox().unwrap().unwrap()
    }

    /// The Inbox of a human assigned `acme/web#482` alone, which is `as_found`.
    fn inbox_of_one_assignment(&mut self, as_found: serde_json::Value) -> Vec<InboxItem> {
        self.search("review-requested:@me", NONE_FOUND);
        self.only_found("assignee:@me", as_found);
        self.github().inbox().unwrap().unwrap()
    }

    /// The search for `filter` finds `acme/web#482` alone, with `as_found`'s fields.
    fn only_found(&mut self, filter: &str, as_found: serde_json::Value) -> Mock {
        let mut node = serde_json::json!({
            "number": 482,
            "title": "Add SSO login",
            "url": "https://github.com/acme/web/pull/482",
            "createdAt": "2026-11-12T08:00:00Z",
            "updatedAt": "2026-11-12T09:30:00Z",
            "author": { "login": "maria" },
            "repository": { "nameWithOwner": "acme/web" },
            "reviewDecision": "REVIEW_REQUIRED",
            "latestOpinionatedReviews": { "nodes": [] },
            "viewerDidAuthor": false,
            "viewerLatestReview": null,
        });
        for (field, value) in as_found.as_object().expect("fields of a node") {
            node[field] = value.clone();
        }
        self.search(filter, &serde_json::json!({ "data": { "search": { "nodes": [node] } } }).to_string())
    }
}

const NONE_FOUND: &str = r#"{"data":{"search":{"nodes":[]}}}"#;

fn inbox_item(repo: &str, number: u64, title: &str, author: &str, role: InboxRole) -> InboxItem {
    InboxItem {
        id: PullRequestRef { host: "github.com".into(), repo: repo.into(), number },
        kind: ForgeKind::GitHub,
        url: format!("https://github.com/{repo}/pull/{number}"),
        title: title.into(),
        author: author.into(),
        created_at: 0,
        updated_at: 0,
        role,
        approved: false,
        verdict_given: false,
    }
}

#[test]
fn the_inbox_lists_review_requests_then_assignments_once_each() {
    let mut h = Harness::new();
    let github = h.github();
    let requested = h.search("review-requested:@me", &fixture("search-review-requested.json"));
    let assigned = h.search("assignee:@me", &fixture("search-assigned.json"));

    let inbox = github.inbox().unwrap().unwrap();

    requested.assert();
    assigned.assert();
    let listed: Vec<_> = inbox.iter().map(|i| InboxItem { created_at: 0, updated_at: 0, ..i.clone() }).collect();
    assert_eq!(
        listed,
        vec![
            inbox_item("acme/web", 482, "Add SSO login", "maria", InboxRole::Reviewer),
            inbox_item("acme/api", 311, "Retry 429 responses with backoff", "lee", InboxRole::Reviewer),
            inbox_item("acme/docs", 45, "Fix broken links in setup guide", "sam", InboxRole::Assignee),
        ]
    );
}

#[test]
fn inbox_times_are_seconds_since_the_epoch() {
    let mut h = Harness::new();
    let github = h.github();
    h.search("review-requested:@me", &fixture("search-review-requested.json"));
    h.search("assignee:@me", NONE_FOUND);

    let inbox = github.inbox().unwrap().unwrap();

    // 2026-11-12T08:00:00Z and 2026-11-12T09:30:00Z.
    assert_eq!((inbox[0].created_at, inbox[0].updated_at), (1_794_470_400, 1_794_475_800));
    // 2026-11-11T16:45:10Z.
    assert_eq!(inbox[1].updated_at, 1_794_415_510);
}

#[test]
fn a_pull_request_is_approved_by_its_review_decision_or_one_approval_without_a_requirement() {
    use serde_json::json;
    // (GitHub's review decision, the latest Approve or Request changes of each reviewer) and
    // whether it's approved. The decision is null when the project requires no review.
    let cases = [
        (json!("APPROVED"), vec!["APPROVED", "APPROVED"], true),
        // Not enough approvals for the project's rules.
        (json!("REVIEW_REQUIRED"), vec!["APPROVED"], false),
        (json!("CHANGES_REQUESTED"), vec!["APPROVED", "CHANGES_REQUESTED"], false),
        (json!(null), vec!["CHANGES_REQUESTED", "APPROVED"], true),
        (json!(null), vec!["CHANGES_REQUESTED"], false),
        (json!(null), vec![], false),
    ];
    for (decision, latest, approved) in cases {
        let mut h = Harness::new();
        let reviews: Vec<_> = latest.iter().map(|state| json!({ "state": state })).collect();

        let inbox = h.inbox_of_one_review_request(json!({
            "reviewDecision": decision,
            "latestOpinionatedReviews": { "nodes": reviews },
        }));

        assert_eq!(inbox[0].approved, approved, "review decision {decision}, latest reviews {latest:?}");
    }
}

#[test]
fn an_assignment_the_human_reviewed_is_done() {
    use serde_json::json;
    // The human's latest review, and whether it gave a Verdict.
    let cases = [
        (json!(null), false),
        (json!({ "state": "APPROVED" }), true),
        (json!({ "state": "CHANGES_REQUESTED" }), true),
        (json!({ "state": "COMMENTED" }), true),
        // Not submitted yet.
        (json!({ "state": "PENDING" }), false),
        // A Verdict taken back no longer counts.
        (json!({ "state": "DISMISSED" }), false),
    ];
    for (review, done) in cases {
        let mut h = Harness::new();

        let inbox = h.inbox_of_one_assignment(json!({ "viewerLatestReview": review }));

        assert_eq!(inbox[0].role, InboxRole::Assignee);
        assert_eq!(inbox[0].verdict_given, done, "latest review {review}");
    }
}

#[test]
fn the_human_s_own_assigned_pull_request_stays_whatever_they_reply() {
    let mut h = Harness::new();

    let inbox = h.inbox_of_one_assignment(serde_json::json!({
        "viewerDidAuthor": true,
        "viewerLatestReview": { "state": "COMMENTED" },
    }));

    assert!(!inbox[0].verdict_given, "no one can request the author's review, so it would never come back");
}

#[test]
fn a_review_requested_again_is_to_do_whatever_the_human_s_earlier_review() {
    let mut h = Harness::new();

    let inbox = h.inbox_of_one_review_request(serde_json::json!({ "viewerLatestReview": { "state": "APPROVED" } }));

    assert!(!inbox[0].verdict_given, "GitHub removes a review request once the human reviews");
}

// --- github.com when it's there by default ---

/// The adapter `adapters` builds for github.com from the config `toml`, with no credentials.
fn github_com_from_config(toml: &str) -> Box<dyn ForgeAdapter> {
    let dirs = TestDirs::new();
    let file = dirs.path().join("config.toml");
    std::fs::write(&file, toml).unwrap();
    let settings = Settings::load(&file).unwrap();

    adapters(&settings.hosts, &no_credentials())
        .into_iter()
        .find(|f| f.recognise("https://github.com/acme/web/pull/482").is_some())
        .expect("an adapter for github.com")
}

#[test]
fn default_github_com_without_credentials_is_skipped_when_another_host_is_configured() {
    let github = github_com_from_config("[[hosts]]\nkind = \"gitlab\"\nhost = \"gitlab.example.com\"\n");

    let inbox = github.inbox();

    assert!(matches!(inbox, Ok(None)), "{inbox:?}");
}

#[test]
fn default_github_com_without_credentials_says_how_to_log_in_when_it_is_the_only_host() {
    let github = github_com_from_config("untracked = false\n");

    let inbox = github.inbox();

    assert!(matches!(&inbox, Err(ForgeError::NoCredentials { host, .. }) if host == "github.com"), "{inbox:?}");
}

#[test]
fn configured_github_com_without_credentials_says_how_to_log_in_next_to_other_hosts() {
    let github = github_com_from_config(
        "[[hosts]]\nkind = \"github\"\nhost = \"github.com\"\n\n\
         [[hosts]]\nkind = \"gitlab\"\nhost = \"gitlab.example.com\"\n",
    );

    let inbox = github.inbox();

    assert!(matches!(&inbox, Err(ForgeError::NoCredentials { host, .. }) if host == "github.com"), "{inbox:?}");
}

#[test]
fn default_github_com_skipped_from_the_inbox_still_says_how_to_log_in_for_a_pasted_url() {
    let github = github_com_from_config("[[hosts]]\nkind = \"gitlab\"\nhost = \"gitlab.example.com\"\n");

    let error = github.pull_request(&pr_ref()).unwrap_err();

    assert!(matches!(&error, ForgeError::NoCredentials { host, .. } if host == "github.com"), "{error:?}");
}

#[test]
fn optional_github_com_with_a_token_reports_a_failing_inbox() {
    let mut h = Harness::new();
    let github = h.github().optional();
    h.server.mock("POST", "/graphql").with_status(401).create();

    let inbox = github.inbox();

    assert!(matches!(&inbox, Err(ForgeError::Unauthorized { .. })), "{inbox:?}");
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
