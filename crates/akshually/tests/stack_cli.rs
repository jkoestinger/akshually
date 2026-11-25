//! Seam B: an agent co-reviewing a Stack of Pull Requests with `akshually draft` and
//! `akshually review --pr`, the human answering through seam A (ADR 0004).

mod support;

use std::process::{Child, Output};

use akshually_core::{SessionView, ThreadId, ThreadRef, Verdict};
use support::{HostedApp, finish, stack_pr_ref, stack_url, still_running};

fn url(number: u64) -> String {
    stack_url(number)
}

fn pr_ref(number: u64) -> akshually_core::forge::PullRequestRef {
    stack_pr_ref(number)
}

fn hosted() -> HostedApp {
    HostedApp::with_stack()
}

fn run(app: &HostedApp, args: &[&str]) -> Output {
    app.akshually(&std::env::temp_dir(), args).output().unwrap()
}

fn stdout(out: &Output) -> String {
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout.clone()).unwrap()
}

/// The agent's `review --pr` on the Stack, through #1's URL, with `extra` arguments.
fn review(app: &HostedApp, extra: &[&str]) -> Child {
    let first = url(1);
    let mut args = vec!["review", "--pr", first.as_str()];
    args.extend_from_slice(extra);
    app.akshually(&std::env::temp_dir(), &args).spawn().unwrap()
}

/// The Stack's Session once the agent waits on it.
fn waiting(app: &HostedApp) -> SessionView {
    app.wait_for_co_reviewer()
}

fn publish(app: &HostedApp, session: &SessionView, number: u64, verdict: Verdict) {
    app.service.select_member(&session.id, &pr_ref(number)).unwrap();
    app.service.publish(&session.id, verdict, "Looked it over.").unwrap();
}

/// The agent drafted a Thread on each member, and the human questioned both in Round 1.
fn round_on_both(extra: &[&str]) -> Output {
    let app = hosted();
    stdout(&run(&app, &["draft", "--pr", &url(1), "--lines", "f1.rs:2", "One."]));
    stdout(&run(&app, &["draft", "--pr", &url(2), "--file", "f2.rs", "Two."]));
    let child = review(&app, extra);
    let session = waiting(&app);
    app.wait_for_review(&session.id);
    for thread in ["T1", "T2"] {
        app.service.reply_in_thread(&ThreadRef::new(&session.id, &ThreadId::new(thread)), "Why?").unwrap();
    }
    app.service.hand_over_round(&session.id).unwrap();
    finish(child)
}

#[test]
fn a_round_on_a_stack_names_the_pull_request_of_each_thread_in_its_heading() {
    let out = round_on_both(&[]);

    let text = stdout(&out);
    assert!(text.contains("\n## T1 · acme/web #1 · f1.rs, new line 2\n"), "{text}");
    assert!(text.contains("\n## T2 · acme/web #2 · f2.rs, whole file\n"), "{text}");
}

#[test]
fn a_round_on_a_stack_gives_each_json_thread_its_pull_requests_url() {
    let out = round_on_both(&["--format", "json"]);

    let round: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let urls: Vec<_> = round["threads"].as_array().unwrap().iter().map(|t| t["pull_request"].clone()).collect();
    assert_eq!(urls, vec![serde_json::json!(url(1)), serde_json::json!(url(2))]);
}

#[test]
fn review_pr_keeps_waiting_when_one_of_two_members_is_published() {
    let app = hosted();
    let mut child = review(&app, &[]);
    let session = waiting(&app);

    publish(&app, &session, 1, Verdict::Approve);

    assert!(still_running(&mut child), "review returned before the Session ended");
    app.service.close(&session.id).unwrap();
    finish(child);
}

#[test]
fn review_pr_lists_each_member_in_stack_order_when_both_are_published() {
    let app = hosted();
    let child = review(&app, &[]);
    let session = waiting(&app);

    publish(&app, &session, 2, Verdict::Feedback);
    publish(&app, &session, 1, Verdict::Approve);
    let out = finish(child);

    let first = url(1);
    let second = url(2);
    assert_eq!(
        stdout(&out),
        format!(
            "Published: the human published their Review of at least one Pull Request of this Stack, as follows.\n\
             - acme/web #1: Published with the Verdict approve: {first}#review\n\
             - acme/web #2: Published with the Verdict feedback: {second}#review\n"
        )
    );
}

#[test]
fn review_pr_says_the_member_the_human_left_was_not_published_once_the_session_closes() {
    let app = hosted();
    let child = review(&app, &[]);
    let session = waiting(&app);
    publish(&app, &session, 2, Verdict::Approve);

    app.service.close(&session.id).unwrap();
    let out = finish(child);

    let second = url(2);
    assert_eq!(
        stdout(&out),
        format!(
            "Published: the human published their Review of at least one Pull Request of this Stack, as follows.\n\
             - acme/web #1: not Published\n\
             - acme/web #2: Published with the Verdict approve: {second}#review\n"
        )
    );
}

#[test]
fn review_pr_says_abandoned_when_the_session_closes_with_no_member_published() {
    let app = hosted();
    let child = review(&app, &[]);
    let session = waiting(&app);

    app.service.close(&session.id).unwrap();
    let out = finish(child);

    assert!(stdout(&out).starts_with("Abandoned: "), "{}", String::from_utf8_lossy(&out.stdout));
}

#[test]
fn review_pr_json_lists_each_member_when_both_are_published() {
    let app = hosted();
    let child = review(&app, &["--format", "json"]);
    let session = waiting(&app);

    publish(&app, &session, 1, Verdict::Approve);
    publish(&app, &session, 2, Verdict::Feedback);
    let out = finish(child);

    let outcome: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(outcome["outcome"], "published");
    assert_eq!(
        outcome["pull_requests"],
        serde_json::json!([
            { "name": "acme/web #1", "url": url(1), "verdict": "approve", "review": format!("{}#review", url(1)) },
            {
                "name": "acme/web #2", "url": url(2),
                "verdict": "feedback", "review": format!("{}#review", url(2)),
            },
        ])
    );
}

#[test]
fn review_pr_json_lists_the_member_that_was_not_published() {
    let app = hosted();
    let child = review(&app, &["--format", "json"]);
    let session = waiting(&app);
    publish(&app, &session, 1, Verdict::Approve);

    app.service.close(&session.id).unwrap();
    let out = finish(child);

    let outcome: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(outcome["outcome"], "published");
    assert_eq!(outcome["pull_requests"][1], serde_json::json!({ "name": "acme/web #2", "url": url(2) }), "{outcome}");
}

#[test]
fn review_pr_json_lists_every_member_when_the_session_is_abandoned() {
    let app = hosted();
    let child = review(&app, &["--format", "json"]);
    let session = waiting(&app);

    app.service.close(&session.id).unwrap();
    let out = finish(child);

    let outcome: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        outcome,
        serde_json::json!({
            "outcome": "abandoned",
            "pull_requests": [
                { "name": "acme/web #1", "url": url(1) },
                { "name": "acme/web #2", "url": url(2) },
            ],
        })
    );
}

#[test]
fn draft_on_a_published_member_exits_1_saying_it_was_published() {
    let app = hosted();
    let child = review(&app, &[]);
    let session = waiting(&app);
    publish(&app, &session, 1, Verdict::Approve);

    let drafted = run(&app, &["draft", "--pr", &url(1), "--review", "Late thought."]);

    assert_eq!(drafted.status.code(), Some(1));
    let error = String::from_utf8_lossy(&drafted.stderr).to_lowercase();
    assert!(error.contains("published"), "{error}");
    app.service.close(&session.id).unwrap();
    finish(child);
}

#[test]
fn reply_and_resolve_on_a_published_member_exit_1_saying_it_was_published() {
    let app = hosted();
    stdout(&run(&app, &["draft", "--pr", &url(1), "--review", "Fine."]));
    let child = review(&app, &[]);
    let session = waiting(&app);
    app.service.keep_draft(&ThreadRef::new(&session.id, &ThreadId::new("T1"))).unwrap();
    publish(&app, &session, 1, Verdict::Approve);

    let replied = run(&app, &["reply", "--pr", &url(1), "T1", "More."]);
    let resolved = run(&app, &["resolve", "--pr", &url(1), "T1"]);

    for out in [replied, resolved] {
        assert_eq!(out.status.code(), Some(1));
        let error = String::from_utf8_lossy(&out.stderr).to_lowercase();
        assert!(error.contains("published"), "{error}");
    }
    app.service.close(&session.id).unwrap();
    finish(child);
}
