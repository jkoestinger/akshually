//! Seam B: how `review` prints the Thread Anchor kinds and code suggestions of a Round.

mod support;

use akshually_core::{Anchor, NewComment, Side};
use akshually_testkit::TestRepo;
use support::{HostedApp, finish};

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
         The human handed over 1 Thread. Address each one, then run `akshually review` again.\n\
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
             Suggested change: replace new lines 2-3 of lib.rs with\n\
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
