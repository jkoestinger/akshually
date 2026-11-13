//! Seam B: how `review` prints the Thread Anchor kinds and code suggestions of a Round.

mod support;

use std::process::Child;

use akshually_core::{Anchor, NewComment, SessionId, Side, ThreadId, ThreadRef, Turn};
use akshually_testkit::{TestRepo, wait_until};
use support::{HostedApp, TIMEOUT, finish};

const EXIT_ROUND: i32 = 4;

fn repo_with_work() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("lib.rs", "fn a() {}\n").commit_all("base");
    repo.branch("agent").write("lib.rs", "fn a() {}\nfn b() {}\nfn c() {}\n");
    repo
}

#[test]
fn a_file_thread_is_headed_by_its_file_without_code() {
    let app = HostedApp::start();
    let repo = repo_with_work();
    let child = app.akshually(repo.path(), &["review"]).spawn().unwrap();
    let session = app.wait_for_session(repo.path());
    app.service.comment(&session.id, Anchor::file("lib.rs"), "Split this file.").unwrap();

    app.service.hand_over_round(&session.id).unwrap();

    let out = finish(child);
    assert_eq!(out.status.code(), Some(EXIT_ROUND));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "# Round 1\n\
         \n\
         The human handed over 1 Thread. Address each one, answer it with `akshually reply <thread> \"…\"` \
         or `akshually resolve <thread>`, then run `akshually review` again.\n\
         \n\
         ## T1 · lib.rs, whole file\n\
         \n\
         Split this file.\n"
    );
}

#[test]
fn a_suggestion_follows_its_comment_as_the_replacement_for_the_anchored_lines() {
    let app = HostedApp::start();
    let repo = repo_with_work();
    let child = app.akshually(repo.path(), &["review"]).spawn().unwrap();
    let session = app.wait_for_session(repo.path());
    let comment = NewComment::new("Merge these.").suggest("fn bc() {}\n");
    app.service.comment(&session.id, Anchor::lines(Side::New, "lib.rs", 2..=3), comment).unwrap();

    app.service.hand_over_round(&session.id).unwrap();

    let out = finish(child);
    assert_eq!(out.status.code(), Some(EXIT_ROUND));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.ends_with(
            "## T1 · lib.rs, new lines 2-3\n\
             \n\
             ```\n\
             fn b() {}\n\
             fn c() {}\n\
             ```\n\
             \n\
             Merge these.\n\
             \n\
             Suggested change to lib.rs, new lines 2-3:\n\
             \n\
             ```\n\
             fn bc() {}\n\
             ```\n"
        ),
        "{stdout}"
    );
}

#[test]
fn a_review_thread_is_headed_as_the_whole_review() {
    let app = HostedApp::start();
    let repo = repo_with_work();
    let child = app.akshually(repo.path(), &["review"]).spawn().unwrap();
    let session = app.wait_for_session(repo.path());
    app.service.comment(&session.id, Anchor::Review, "Add a changelog entry.").unwrap();

    app.service.hand_over_round(&session.id).unwrap();

    let out = finish(child);
    assert_eq!(out.status.code(), Some(EXIT_ROUND));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.ends_with("\n## T1 · whole Review\n\nAdd a changelog entry.\n"), "{stdout}");
}

#[test]
fn an_outdated_thread_is_marked_and_shows_the_code_it_was_written_against() {
    let app = HostedApp::start();
    let repo = repo_with_work();
    let mut first = app.akshually(repo.path(), &["review"]).spawn().unwrap();
    let session = app.wait_for_session(repo.path());
    app.service.comment(&session.id, Anchor::lines(Side::New, "lib.rs", 2..=3), "Why two?").unwrap();
    // The agent stops waiting and rewrites the code before the human hands the Round over.
    first.kill().unwrap();
    finish(first);
    repo.write("lib.rs", "fn a() {}\nfn everything_else() {}\n");
    let second = app.akshually(repo.path(), &["review"]).spawn().unwrap();
    wait_until(TIMEOUT, "the Thread to go Outdated", || {
        app.service.session(&session.id).filter(|s| s.threads[0].outdated)
    });

    app.service.hand_over_round(&session.id).unwrap();

    let out = finish(second);
    assert_eq!(out.status.code(), Some(EXIT_ROUND));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.ends_with(
            "## T1 · lib.rs, new lines 2-3 · Outdated\n\
             \n\
             This code is no longer in the Changeset; the Thread was written against it:\n\
             \n\
             ```\n\
             fn b() {}\n\
             fn c() {}\n\
             ```\n\
             \n\
             Why two?\n"
        ),
        "{stdout}"
    );
}

/// Round 1 handed the agent T1 (new line 3) and T2 (new line 2); the agent resolved T1, and
/// its next `review` call, with `args`, is waiting.
fn after_the_agent_resolved_t1(app: &HostedApp, repo: &TestRepo, args: &[&str]) -> (SessionId, Child) {
    let first = app.akshually(repo.path(), &["review"]).spawn().unwrap();
    let session = app.wait_for_session(repo.path());
    app.service.comment(&session.id, Anchor::lines(Side::New, "lib.rs", 3..=3), "Drop c").unwrap();
    app.service.comment(&session.id, Anchor::lines(Side::New, "lib.rs", 2..=2), "Rename b").unwrap();
    app.service.hand_over_round(&session.id).unwrap();
    assert_eq!(finish(first).status.code(), Some(EXIT_ROUND));
    app.service.resolve(repo.path(), &ThreadId::new("T1")).unwrap();
    let second = app.akshually(repo.path(), args).spawn().unwrap();
    wait_until(TIMEOUT, "the next review call", || app.service.session(&session.id).filter(|s| s.turn == Turn::Human));
    (session.id, second)
}

#[test]
fn a_reopened_thread_and_a_reply_in_a_thread_go_out_under_their_threads() {
    let app = HostedApp::start();
    let repo = repo_with_work();
    let (session, second) = after_the_agent_resolved_t1(&app, &repo, &["review"]);
    app.service.reopen_thread(&ThreadRef::new(&session, &ThreadId::new("T1"))).unwrap();
    app.service.reply_in_thread(&ThreadRef::new(&session, &ThreadId::new("T2")), "To what?").unwrap();

    app.service.hand_over_round(&session).unwrap();

    let out = finish(second);
    assert_eq!(out.status.code(), Some(EXIT_ROUND));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.ends_with(
            "## T1 · lib.rs, new line 3 · Reopened\n\
             \n\
             The human reopened this Thread: it isn't dealt with yet.\n\
             \n\
             ```\n\
             fn c() {}\n\
             ```\n\
             \n\
             ## T2 · lib.rs, new line 2\n\
             \n\
             ```\n\
             fn b() {}\n\
             ```\n\
             \n\
             To what?\n"
        ),
        "{stdout}"
    );
}

#[test]
fn a_reopened_thread_is_flagged_in_json() {
    let app = HostedApp::start();
    let repo = repo_with_work();
    let (session, second) = after_the_agent_resolved_t1(&app, &repo, &["review", "--format", "json"]);
    app.service.reopen_thread(&ThreadRef::new(&session, &ThreadId::new("T1"))).unwrap();

    app.service.hand_over_round(&session).unwrap();

    let out = finish(second);
    assert_eq!(out.status.code(), Some(EXIT_ROUND));
    let round: serde_json::Value = serde_json::from_slice(&out.stdout).expect("stdout is JSON");
    assert_eq!(round["threads"][0]["id"], "T1");
    assert_eq!(round["threads"][0]["reopened"], true);
    assert_eq!(round["threads"][0]["comments"], serde_json::json!([]));
}
