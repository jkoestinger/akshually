//! Seam B, the CLI contract: parallel Sessions in one app instance.

mod support;

use akshually_testkit::TestRepo;
use support::{HostedApp, finish, still_running};

#[test]
fn agents_in_two_worktrees_get_two_sessions_in_one_app_and_each_waits_for_its_own_verdict() {
    let app = HostedApp::start();
    let repo = TestRepo::new();
    repo.write("lib.rs", "fn a() {}\n").commit_all("base");
    let one = repo.worktree("agent-one");
    one.write("lib.rs", "fn one() {}\n");
    let two = repo.worktree("agent-two");
    two.write("lib.rs", "fn two() {}\n");

    let first = app.akshually(one.path(), &["review"]).spawn().unwrap();
    let a = app.wait_for_session(one.path());
    let mut second = app.akshually(two.path(), &["review"]).spawn().unwrap();
    let b = app.wait_for_session(two.path());

    assert_ne!(a.id, b.id);
    assert_eq!((a.branch(), b.branch()), ("agent-one", "agent-two"));
    assert_eq!(app.service.displayed(), Some(a.id.clone()), "the second Session doesn't take the view");
    assert!(app.service.session(&b.id).unwrap().attention);

    app.service.approve(&a.id).unwrap();
    assert_eq!(finish(first).status.code(), Some(0));
    assert!(still_running(&mut second), "approving one Session leaves the other waiting");
    app.service.approve(&b.id).unwrap();
    assert_eq!(finish(second).status.code(), Some(0));
}
