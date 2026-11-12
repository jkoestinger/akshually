//! Seam B: an agent co-reviewing a Pull Request with `akshually draft` and
//! `akshually review --pr`, the human answering through seam A.

mod support;

use std::io::Write;
use std::process::{Output, Stdio};

use akshually_core::{Anchor, Outcome, Party, Side, Verdict};
use support::{HostedApp, PR_URL, finish, still_running};

fn run(app: &HostedApp, args: &[&str]) -> Output {
    app.akshually(&std::env::temp_dir(), args).output().unwrap()
}

fn stdout(out: &Output) -> String {
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout.clone()).unwrap()
}

#[test]
fn draft_adds_agent_drafts_with_each_anchor_kind_and_prints_their_threads() {
    let app = HostedApp::with_pull_request();

    let lines =
        run(&app, &["draft", "--pr", PR_URL, "--lines", "login.rs:2", "Name it.\n\n```suggestion\n    okta();\n```\n"]);
    let old = run(&app, &["draft", "--pr", PR_URL, "--lines", "login.rs:1-1", "--old", "Why remove it?"]);
    let file = run(&app, &["draft", "--pr", PR_URL, "--file", "README.md", "Link the docs."]);
    let review = run(&app, &["draft", "--pr", PR_URL, "--review", "Small and clear."]);

    assert_eq!([&lines, &old, &file, &review].map(stdout), ["T1\n", "T2\n", "T3\n", "T4\n"].map(String::from));
    let session = app.service.sessions().pop().unwrap();
    let anchors: Vec<_> = session.threads.iter().map(|t| t.anchor.clone()).collect();
    assert_eq!(
        anchors,
        vec![
            Anchor::lines(Side::New, "login.rs", 2..=2),
            Anchor::lines(Side::Old, "login.rs", 1..=1),
            Anchor::file("README.md"),
            Anchor::Review,
        ]
    );
    let first = &session.threads[0].comments[0];
    assert_eq!((first.author.clone(), first.body.as_str()), (Party::Agent, "Name it."));
    assert_eq!(first.suggestion.as_deref(), Some("    okta();\n"));
    assert!(session.threads.iter().all(|t| t.comments[0].is_agent_draft()));
}

#[test]
fn draft_reads_its_body_from_stdin_when_given_a_dash() {
    let app = HostedApp::with_pull_request();
    let mut child = app
        .akshually(&std::env::temp_dir(), &["draft", "--pr", PR_URL, "--review", "-"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"Line one.\n\nLine `two`.\n").unwrap();

    stdout(&finish(child));

    let session = app.service.sessions().pop().unwrap();
    assert_eq!(session.threads[0].comments[0].body, "Line one.\n\nLine `two`.\n");
}

#[test]
fn a_draft_the_service_refuses_exits_1_with_the_reason() {
    let app = HostedApp::with_pull_request();

    let out = run(&app, &["draft", "--pr", PR_URL, "--lines", "nowhere.rs:1", "Hm."]);

    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("nowhere.rs"), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn review_pr_blocks_until_the_human_publishes_then_exits_5_with_the_verdict_and_link() {
    let app = HostedApp::with_pull_request();
    stdout(&run(&app, &["draft", "--pr", PR_URL, "--file", "README.md", "Link the docs."]));
    let mut child = app.akshually(&std::env::temp_dir(), &["review", "--pr", PR_URL]).spawn().unwrap();
    let session = app.wait_for_co_reviewer();

    assert!(still_running(&mut child), "blocks until the human is done");
    app.service.keep_draft(&session.id, &session.threads[0].id).unwrap();
    app.service.publish(&session.id, Verdict::RequestChanges, "").unwrap();
    let out = finish(child);

    assert_eq!(out.status.code(), Some(5), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!("Published with the Verdict request changes: {PR_URL}#pullrequestreview-1\n")
    );
}

#[test]
fn review_pr_prints_the_published_outcome_as_json() {
    let app = HostedApp::with_pull_request();
    let child = app.akshually(&std::env::temp_dir(), &["review", "--pr", PR_URL, "--format", "json"]).spawn().unwrap();
    let session = app.wait_for_co_reviewer();

    app.service.publish(&session.id, Verdict::Approve, "").unwrap();
    let out = finish(child);

    assert_eq!(out.status.code(), Some(5));
    let outcome: Outcome = serde_json::from_slice(&out.stdout).unwrap();
    let url = format!("{PR_URL}#pullrequestreview-1");
    assert_eq!(outcome, Outcome::Published { verdict: Verdict::Approve, url });
}

#[test]
fn review_pr_exits_3_when_the_human_abandons_the_co_review() {
    let app = HostedApp::with_pull_request();
    let child = app.akshually(&std::env::temp_dir(), &["review", "--pr", PR_URL]).spawn().unwrap();
    let session = app.wait_for_co_reviewer();

    app.service.close(&session.id).unwrap();
    let out = finish(child);

    assert_eq!(out.status.code(), Some(3), "stderr: {}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn a_second_review_pr_while_one_waits_fails_as_busy() {
    let app = HostedApp::with_pull_request();
    let mut first = app.akshually(&std::env::temp_dir(), &["review", "--pr", PR_URL]).spawn().unwrap();
    app.wait_for_co_reviewer();

    let second = run(&app, &["review", "--pr", PR_URL]);

    assert_eq!(second.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&second.stderr).contains("already waiting"));
    assert!(still_running(&mut first));
    first.kill().unwrap();
}
