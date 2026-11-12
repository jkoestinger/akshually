//! Seam C: Publish through the GitHub adapter, against a mock GitHub API. Nothing here reaches
//! the network or real credentials.

use std::collections::HashMap;

use akshually_core::forge::{
    CredentialSources, ForgeAdapter, ForgeError, ForgeKind, GitHub, HostConfig, PullRequest, PullRequestRef,
    RemoteAnchor, Review, ReviewComment, ReviewReply,
};
use akshually_core::{Side, Verdict};
use mockito::{Matcher, Mock, Server, ServerGuard};
use serde_json::{Value, json};

const TOKEN: &str = "gho_test";

/// A mock GitHub API, and the adapter for github.com pointed at it, with a token from the
/// environment.
struct Harness {
    server: ServerGuard,
}

impl Harness {
    fn new() -> Self {
        Harness { server: Server::new() }
    }

    fn github(&self) -> GitHub {
        let host = HostConfig { api_base: Some(self.server.url()), ..HostConfig::new(ForgeKind::GitHub, "github.com") };
        let env = HashMap::from([("GITHUB_TOKEN".to_owned(), TOKEN.to_owned())]);
        GitHub::new(host, CredentialSources { gh: "/nonexistent/gh".into(), glab: "/nonexistent/glab".into(), env })
    }

    /// `GET /user`, answering the token's login.
    fn viewer(&mut self, login: &str) -> Mock {
        self.server
            .mock("GET", "/user")
            .match_header("authorization", format!("Bearer {TOKEN}").as_str())
            .with_header("content-type", "application/json")
            .with_body(format!(r#"{{ "login": "{login}", "id": 1 }}"#))
            .create()
    }
}

fn pull_request(author: &str) -> PullRequest {
    PullRequest {
        id: PullRequestRef { host: "github.com".into(), repo: "acme/web".into(), number: 482 },
        url: "https://github.com/acme/web/pull/482".into(),
        title: "Add SSO login".into(),
        author: author.into(),
        head_branch: "sso".into(),
        head_sha: "head1".into(),
        base_sha: "base1".into(),
    }
}

// --- Verdicts ---

#[test]
fn every_verdict_is_offered_on_someone_else_s_pull_request() {
    let mut h = Harness::new();
    let viewer = h.viewer("lee");

    let verdicts = h.github().verdicts(&pull_request("maria")).unwrap();

    viewer.assert();
    assert_eq!(verdicts, vec![Verdict::Approve, Verdict::RequestChanges, Verdict::Feedback]);
}

#[test]
fn only_feedback_is_offered_on_your_own_pull_request() {
    let mut h = Harness::new();
    h.viewer("Maria");

    let verdicts = h.github().verdicts(&pull_request("maria")).unwrap();

    assert_eq!(verdicts, vec![Verdict::Feedback], "GitHub refuses to let authors approve or request changes");
}

// --- Publish: one pending review, its Threads, then the Verdict ---

const REVIEW_URL: &str = "https://github.com/acme/web/pull/482#pullrequestreview-77";

impl Harness {
    /// A GraphQL `operation` (a query or mutation field) sent with `variables` among its
    /// variables, answering `data`.
    fn graphql(&mut self, operation: &str, variables: Value, data: Value) -> Mock {
        self.server
            .mock("POST", "/graphql")
            .match_header("authorization", format!("Bearer {TOKEN}").as_str())
            .match_body(Matcher::AllOf(vec![
                Matcher::Regex(format!("{operation}\\(")),
                Matcher::PartialJson(json!({ "variables": variables })),
            ]))
            .with_header("content-type", "application/json")
            .with_body(json!({ "data": data }).to_string())
            .create()
    }

    /// The Pull Request's GraphQL id, `PR_1`.
    fn pull_request_id(&mut self) -> Mock {
        let variables = json!({ "owner": "acme", "repo": "web", "number": 482 });
        self.graphql("pullRequest", variables, json!({ "repository": { "pullRequest": { "id": "PR_1" } } }))
    }

    /// Starts the pending review `PRRV_1` on `head1`.
    fn start_review(&mut self) -> Mock {
        let input = json!({ "input": { "pullRequestId": "PR_1", "commitOID": "head1" } });
        let data = json!({ "addPullRequestReview": { "pullRequestReview": { "id": "PRRV_1" } } });
        self.graphql("addPullRequestReview", input, data)
    }

    /// Adds one Thread to the pending review.
    fn thread(&mut self, mut input: Value) -> Mock {
        input["pullRequestReviewId"] = "PRRV_1".into();
        let data = json!({ "addPullRequestReviewThread": { "thread": { "id": "PRRT_9" } } });
        self.graphql("addPullRequestReviewThread", json!({ "input": input }), data)
    }

    /// Submits the pending review with `event` and `body`.
    fn submit(&mut self, event: &str, body: &str) -> Mock {
        let input = json!({ "input": { "pullRequestReviewId": "PRRV_1", "event": event, "body": body } });
        let data = json!({ "submitPullRequestReview": { "pullRequestReview": { "url": REVIEW_URL } } });
        self.graphql("submitPullRequestReview", input, data)
    }
}

/// A review touching no existing Thread, to fill in.
fn no_threads() -> Review {
    Review {
        verdict: Verdict::Feedback,
        body: String::new(),
        comments: Vec::new(),
        replies: Vec::new(),
        resolve: Vec::new(),
        reopen: Vec::new(),
    }
}

fn comment(anchor: RemoteAnchor, body: &str) -> ReviewComment {
    ReviewComment { anchor, body: body.into(), suggestion: None }
}

fn lines(side: Side, start: u32, end: u32) -> RemoteAnchor {
    RemoteAnchor::Lines { path: "login.rs".into(), side, start, end }
}

#[test]
fn publishes_one_review_with_its_threads_summary_and_verdict() {
    let mut h = Harness::new();
    let sent = [
        h.pull_request_id(),
        h.start_review(),
        h.thread(json!({
            "path": "login.rs", "body": "Why was this removed?", "subjectType": "LINE",
            "line": 1, "side": "LEFT",
        })),
        h.thread(json!({
            "path": "login.rs", "body": "Extract this.", "subjectType": "LINE",
            "startLine": 2, "startSide": "RIGHT", "line": 3, "side": "RIGHT",
        })),
        h.thread(json!({ "path": "README.md", "body": "Link the docs.", "subjectType": "FILE" })),
        h.submit("REQUEST_CHANGES", "A few things first."),
    ];
    let review = Review {
        verdict: Verdict::RequestChanges,
        body: "A few things first.".into(),
        comments: vec![
            comment(lines(Side::Old, 1, 1), "Why was this removed?"),
            comment(lines(Side::New, 2, 3), "Extract this."),
            comment(RemoteAnchor::File { path: "README.md".into() }, "Link the docs."),
        ],
        ..no_threads()
    };

    let url = h.github().publish(&pull_request("maria"), &review).unwrap();

    sent.iter().for_each(Mock::assert);
    assert_eq!(url, REVIEW_URL);
}

#[test]
fn feedback_is_published_as_github_s_comment() {
    let mut h = Harness::new();
    h.pull_request_id();
    h.start_review();
    let submit = h.submit("COMMENT", "Some thoughts.");
    let review =
        Review { verdict: Verdict::Feedback, body: "Some thoughts.".into(), comments: Vec::new(), ..no_threads() };

    h.github().publish(&pull_request("maria"), &review).unwrap();

    submit.assert();
}

#[test]
fn a_suggestion_is_published_as_a_native_suggestion_block() {
    let mut h = Harness::new();
    h.pull_request_id();
    h.start_review();
    h.submit("APPROVE", "");
    let plain = h.thread(json!({
        "path": "login.rs", "line": 2, "side": "RIGHT",
        "body": "Name the provider.\n\n```suggestion\n    okta();\n```",
    }));
    // Code holding a fence gets a longer one; code without a final newline still closes it.
    let fenced = h.thread(json!({
        "path": "login.rs", "startLine": 4, "line": 5, "side": "RIGHT",
        "body": "Document it.\n\n````suggestion\n/// ```\n/// login();\n````",
    }));
    let delete =
        h.thread(json!({ "path": "login.rs", "line": 7, "side": "RIGHT", "body": "Drop this.\n\n```suggestion\n```" }));
    let suggest = |start, end, body: &str, code: &str| ReviewComment {
        anchor: lines(Side::New, start, end),
        body: body.into(),
        suggestion: Some(code.into()),
    };
    let review = Review {
        verdict: Verdict::Approve,
        body: String::new(),
        comments: vec![
            suggest(2, 2, "Name the provider.", "    okta();\n"),
            suggest(4, 5, "Document it.", "/// ```\n/// login();"),
            suggest(7, 7, "Drop this.", ""),
        ],
        ..no_threads()
    };

    h.github().publish(&pull_request("maria"), &review).unwrap();

    [plain, fenced, delete].iter().for_each(Mock::assert);
}

#[test]
fn a_thread_github_refuses_deletes_the_pending_review_and_submits_nothing() {
    let mut h = Harness::new();
    h.pull_request_id();
    h.start_review();
    let refused = h
        .server
        .mock("POST", "/graphql")
        .match_body(Matcher::Regex("addPullRequestReviewThread\\(".into()))
        .with_body(r#"{ "data": { "addPullRequestReviewThread": null }, "errors": [{ "message": "Line could not be resolved" }] }"#)
        .create();
    let submit = h
        .server
        .mock("POST", "/graphql")
        .match_body(Matcher::Regex("submitPullRequestReview\\(".into()))
        .expect(0)
        .create();
    let deleted = h.graphql(
        "deletePullRequestReview",
        json!({ "input": { "pullRequestReviewId": "PRRV_1" } }),
        json!({ "deletePullRequestReview": { "pullRequestReview": { "id": "PRRV_1" } } }),
    );
    let review = Review {
        verdict: Verdict::Approve,
        body: String::new(),
        comments: vec![comment(lines(Side::New, 40, 40), "Out of the diff.")],
        ..no_threads()
    };

    let error = h.github().publish(&pull_request("maria"), &review).unwrap_err();

    [refused, submit, deleted].iter().for_each(Mock::assert);
    assert!(matches!(&error, ForgeError::Api { message, .. } if message == "Line could not be resolved"), "{error:?}");
}

// --- Existing Threads: replies in the review, resolving and reopening ---

impl Harness {
    /// Adds a reply in the existing Thread `thread` to the pending review.
    fn reply(&mut self, thread: &str, body: &str) -> Mock {
        let input = json!({ "pullRequestReviewId": "PRRV_1", "pullRequestReviewThreadId": thread, "body": body });
        let data = json!({ "addPullRequestReviewThreadReply": { "comment": { "id": "PRRC_5" } } });
        self.graphql("addPullRequestReviewThreadReply", json!({ "input": input }), data)
    }
}

#[test]
fn replies_go_into_the_pending_review() {
    let mut h = Harness::new();
    let sent = [
        h.pull_request_id(),
        h.start_review(),
        h.reply("PRRT_1", "Agreed."),
        h.reply("PRRT_2", "Fixed in head1."),
        h.submit("COMMENT", ""),
    ];
    let replies = vec![
        ReviewReply { thread: "PRRT_1".into(), body: "Agreed.".into() },
        ReviewReply { thread: "PRRT_2".into(), body: "Fixed in head1.".into() },
    ];
    let review = Review { replies, ..no_threads() };

    h.github().publish(&pull_request("maria"), &review).unwrap();

    sent.iter().for_each(Mock::assert);
}

impl Harness {
    /// Marks the Thread `thread` Resolved (`resolveReviewThread`) or reopens it
    /// (`unresolveReviewThread`).
    fn resolution(&mut self, mutation: &str, thread: &str) -> Mock {
        let data = json!({ mutation: { "thread": { "id": thread } } });
        // `\b`: "resolveReviewThread" is also the end of "unresolveReviewThread".
        self.graphql(&format!(r"\b{mutation}"), json!({ "input": { "threadId": thread } }), data)
    }

    /// `mutation` on the Thread `thread`, failing with GitHub's error `message`.
    fn refused(&mut self, mutation: &str, thread: &str, message: &str) -> Mock {
        self.server
            .mock("POST", "/graphql")
            .match_body(Matcher::AllOf(vec![
                Matcher::Regex(format!(r"\b{mutation}\(")),
                Matcher::PartialJson(json!({ "variables": { "input": { "threadId": thread } } })),
            ]))
            .with_body(json!({ "data": { mutation: null }, "errors": [{ "message": message }] }).to_string())
            .create()
    }
}

#[test]
fn threads_are_resolved_and_reopened_with_the_review() {
    let mut h = Harness::new();
    let sent = [
        h.pull_request_id(),
        h.start_review(),
        h.resolution("resolveReviewThread", "PRRT_1"),
        h.resolution("unresolveReviewThread", "PRRT_2"),
        h.submit("APPROVE", ""),
    ];
    let review = Review {
        verdict: Verdict::Approve,
        resolve: vec!["PRRT_1".into()],
        reopen: vec!["PRRT_2".into()],
        ..no_threads()
    };

    h.github().publish(&pull_request("maria"), &review).unwrap();

    sent.iter().for_each(Mock::assert);
}

#[test]
fn a_resolve_github_refuses_undoes_the_others_and_publishes_nothing() {
    let mut h = Harness::new();
    h.pull_request_id();
    h.start_review();
    let resolved = h.resolution("resolveReviewThread", "PRRT_1");
    let refused = h.refused("resolveReviewThread", "PRRT_3", "Could not resolve to a node");
    let undone = h.resolution("unresolveReviewThread", "PRRT_1");
    let submit = h
        .server
        .mock("POST", "/graphql")
        .match_body(Matcher::Regex(r"submitPullRequestReview\(".into()))
        .expect(0)
        .create();
    let deleted = h.graphql(
        "deletePullRequestReview",
        json!({ "input": { "pullRequestReviewId": "PRRV_1" } }),
        json!({ "deletePullRequestReview": { "pullRequestReview": { "id": "PRRV_1" } } }),
    );
    let review = Review { verdict: Verdict::Approve, resolve: vec!["PRRT_1".into(), "PRRT_3".into()], ..no_threads() };

    let error = h.github().publish(&pull_request("maria"), &review).unwrap_err();

    [resolved, refused, undone, submit, deleted].iter().for_each(Mock::assert);
    assert!(matches!(&error, ForgeError::Api { message, .. } if message == "Could not resolve to a node"), "{error:?}");
}
