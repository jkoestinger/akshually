//! Seam B: an agent co-reviewing a Pull Request or local work with `akshually draft` and
//! `akshually review`, the human answering through seam A.

mod support;

use std::io::Write;
use std::process::{Output, Stdio};

use akshually_core::{Anchor, Outcome, Party, Side, ThreadId, ThreadRef, Verdict};
use akshually_testkit::{TestRepo, wait_until};
use support::{HostedApp, PR_URL, TIMEOUT, finish, still_running};

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
    assert!(
        session.threads.iter().all(|t| t.comments[0].is_agent_draft()),
        "a Thread doesn't open with an agent Draft"
    );
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
    app.service.keep_draft(&ThreadRef::new(&session.id, &session.threads[0].id)).unwrap();
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
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(stderr.contains("already waiting"), "stderr: {stderr}");
    assert!(still_running(&mut first), "the first review --pr stopped waiting");
    first.kill().unwrap();
}

#[test]
fn review_pr_exits_4_with_the_humans_questions_and_reply_pr_answers_them() {
    let app = HostedApp::with_pull_request();
    stdout(&run(&app, &["draft", "--pr", PR_URL, "--file", "README.md", "Link the docs."]));
    let child = app.akshually(&std::env::temp_dir(), &["review", "--pr", PR_URL]).spawn().unwrap();
    let session = app.wait_for_co_reviewer();
    let thread = session.threads[0].id.clone();

    app.service.reply_in_thread(&ThreadRef::new(&session.id, &thread), "Which docs?").unwrap();
    app.service.hand_over_round(&session.id).unwrap();
    let out = finish(child);

    assert_eq!(out.status.code(), Some(4), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!(
            "# Round 1\n\
             \n\
             The human handed over 1 Thread. Address each one, answer it with \
             `akshually reply --pr {PR_URL} <thread> \"…\"` or `akshually resolve --pr {PR_URL} <thread>`, \
             then run `akshually review --pr {PR_URL}` again.\n\
             \n\
             ## T1 · README.md, whole file\n\
             \n\
             Which docs?\n"
        )
    );
    stdout(&run(&app, &["reply", "--pr", PR_URL, "T1", "The SSO setup guide."]));
    let answer = app.service.session(&session.id).unwrap().threads[0].comments.last().unwrap().clone();
    assert_eq!((answer.author, answer.body.as_str()), (Party::Agent, "The SSO setup guide."));
}

/// Local work on branch `feature`: `lib.rs` gained `b`.
fn local_work() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("lib.rs", "fn a() {}\n").commit_all("base");
    repo.branch("feature").write("lib.rs", "fn a() {}\nfn b() {}\n").commit_all("add b");
    repo
}

#[test]
fn a_local_co_review_drafts_answers_questions_and_exits_6_when_the_human_closes_it() {
    let app = HostedApp::start();
    let repo = local_work();

    let drafted = app.akshually(repo.path(), &["draft", "--lines", "lib.rs:2", "Name it."]).output().unwrap();
    assert_eq!(stdout(&drafted), "T1\n");
    let child = app.akshually(repo.path(), &["review"]).spawn().unwrap();
    let session = wait_until(TIMEOUT, "the co-Reviewer to wait", || {
        app.service.sessions().into_iter().find(|s| s.repo() == Some(repo.path()) && s.agent_waiting)
    });
    app.service.reply_in_thread(&ThreadRef::new(&session.id, &ThreadId::new("T1")), "Name it what?").unwrap();
    app.service.hand_over_round(&session.id).unwrap();
    let round = finish(child);
    assert_eq!(round.status.code(), Some(4), "stderr: {}", String::from_utf8_lossy(&round.stderr));
    let round_stdout = String::from_utf8_lossy(&round.stdout);
    assert!(round_stdout.ends_with("Name it what?\n"), "{round_stdout}");

    stdout(&app.akshually(repo.path(), &["reply", "T1", "`second`."]).output().unwrap());
    let mut child = app.akshually(repo.path(), &["review"]).spawn().unwrap();
    assert!(still_running(&mut child), "the second review returned before the human acted");
    app.service.close(&session.id).unwrap();
    let out = finish(child);

    assert_eq!(out.status.code(), Some(6), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "Closed: the human finished with your co-review. There is nothing left to do.\n"
    );
    assert!(app.service.sessions().is_empty());
}

#[test]
fn a_closed_local_co_review_prints_closed_as_json() {
    let app = HostedApp::start();
    let repo = local_work();
    stdout(&app.akshually(repo.path(), &["draft", "--review", "Small."]).output().unwrap());
    let session = app.wait_for_session(repo.path());

    app.service.close(&session.id).unwrap();
    let out = app.akshually(repo.path(), &["review", "--format", "json"]).output().unwrap();

    assert_eq!(out.status.code(), Some(6));
    assert_eq!(serde_json::from_slice::<Outcome>(&out.stdout).unwrap(), Outcome::Closed);
}
