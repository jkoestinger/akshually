//! Seam A: the core review service, in-process, against real throwaway git repositories.

use std::time::Duration;

use akshually_core::{FileChange, Outcome, ReviewRequest, Service};
use akshually_testkit::{TestDirs, TestRepo};

fn change(path: &str, old: Option<&str>, new: Option<&str>) -> FileChange {
    FileChange { path: path.into(), old: old.map(Into::into), new: new.map(Into::into) }
}

/// A repository on branch `feature`, which has one commit of its own, while `main` moved on
/// after the branch point, plus uncommitted changes: a staged new file, an unstaged edit and
/// an unstaged deletion.
fn feature_branch_with_uncommitted_work() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("keep.txt", "untouched\n").write("edit.txt", "one\ntwo\n").write("gone.txt", "bye\n").commit_all("base");
    repo.branch("feature").write("edit.txt", "one\nTWO\n").commit_all("agent's commit");
    repo.switch("main").write("main-only.txt", "later\n").commit_all("main moves on");
    repo.switch("feature");
    repo.write("new.txt", "fresh\n").git(&["add", "new.txt"]);
    repo.write("edit.txt", "one\nTWO\nthree\n").remove("gone.txt");
    repo
}

#[test]
fn review_opens_a_session_on_the_branch_against_its_merge_base_plus_uncommitted_changes() {
    let repo = feature_branch_with_uncommitted_work();
    let svc = Service::new();

    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();

    let session = svc.session(pending.session()).expect("the Session is open");
    assert_eq!(session.repo, repo.path());
    assert_eq!(session.branch, "feature");
    assert_eq!(
        session.changeset.files,
        vec![
            change("edit.txt", Some("one\ntwo\n"), Some("one\nTWO\nthree\n")),
            change("gone.txt", Some("bye\n"), None),
            change("new.txt", None, Some("fresh\n")),
        ]
    );
}

#[test]
fn review_from_a_subdirectory_reviews_the_whole_repository() {
    let repo = TestRepo::new();
    repo.write("src/lib.rs", "a\n").commit_all("base");
    repo.write("src/lib.rs", "b\n");
    let svc = Service::new();

    let pending = svc.review(ReviewRequest::new(repo.path().join("src"))).unwrap();

    let session = svc.session(pending.session()).unwrap();
    assert_eq!(session.repo, repo.path());
    assert_eq!(session.changeset.files, vec![change("src/lib.rs", Some("a\n"), Some("b\n"))]);
}

#[test]
fn review_outside_a_repository_fails() {
    let dir = TestDirs::new();
    let svc = Service::new();

    assert!(svc.review(ReviewRequest::new(dir.path())).is_err());
    assert!(svc.sessions().is_empty());
}

#[test]
fn approving_returns_approved_to_the_waiting_review_and_ends_the_session() {
    let repo = feature_branch_with_uncommitted_work();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = pending.session().clone();

    svc.approve(&id).unwrap();

    assert_eq!(pending.wait_timeout(Duration::from_secs(1)), Some(Outcome::Approved));
    assert!(svc.session(&id).is_none());
}

#[test]
fn abandoning_returns_abandoned_to_the_waiting_review_and_ends_the_session() {
    let repo = feature_branch_with_uncommitted_work();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = pending.session().clone();

    svc.abandon(&id).unwrap();

    assert_eq!(pending.wait_timeout(Duration::from_secs(1)), Some(Outcome::Abandoned));
    assert!(svc.session(&id).is_none());
}

#[test]
fn review_blocks_until_the_human_acts() {
    let repo = feature_branch_with_uncommitted_work();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();

    assert_eq!(pending.wait_timeout(Duration::from_millis(50)), None);
}

#[test]
fn reviews_in_two_repositories_are_two_sessions() {
    let (a, b) = (feature_branch_with_uncommitted_work(), feature_branch_with_uncommitted_work());
    let svc = Service::new();

    let pa = svc.review(ReviewRequest::new(a.path())).unwrap();
    let pb = svc.review(ReviewRequest::new(b.path())).unwrap();

    assert_ne!(pa.session(), pb.session());
    assert_eq!(svc.sessions().len(), 2);
}

#[test]
fn subscribers_see_sessions_open_and_end() {
    let repo = feature_branch_with_uncommitted_work();
    let svc = Service::new();
    let (tx, rx) = std::sync::mpsc::channel();
    svc.subscribe(move |sessions| tx.send(sessions.len()).unwrap());

    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    svc.approve(pending.session()).unwrap();

    assert_eq!(rx.try_iter().collect::<Vec<_>>(), vec![1, 0]);
}

#[test]
fn acting_on_an_unknown_session_fails() {
    let repo = feature_branch_with_uncommitted_work();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    svc.approve(pending.session()).unwrap();

    assert!(svc.approve(pending.session()).is_err());
    assert!(svc.abandon(pending.session()).is_err());
}
