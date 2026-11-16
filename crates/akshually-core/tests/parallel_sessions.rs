//! Seam A, the core review service: parallel Sessions, Turn and attention.

use akshually_core::{Anchor, Displayed, Error, ReviewRequest, Service, SessionId, Side};
use akshually_testkit::TestRepo;

/// A repository on branch `agent` with a committed change to `lib.rs`.
fn agent_branch() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("lib.rs", "fn a() {}\n").commit_all("base");
    repo.branch("agent").write("lib.rs", "fn a() {}\nfn b() {}\n").commit_all("agent's work");
    repo
}

#[test]
fn agents_in_two_worktrees_of_one_repository_get_two_sessions() {
    let repo = agent_branch();
    let other = repo.worktree("other-agent");
    other.write("lib.rs", "fn other() {}\n");
    let svc = Service::new();

    let a = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let b = svc.review(ReviewRequest::new(other.path())).unwrap();

    assert_ne!(a.session(), b.session());
    let sessions = svc.sessions();
    let opened: Vec<_> = sessions.iter().map(|s| (s.repo().unwrap(), s.branch())).collect();
    assert_eq!(opened, vec![(repo.path(), "agent"), (other.path(), "other-agent")]);
}

#[test]
fn the_first_session_is_displayed_without_attention() {
    let repo = agent_branch();
    let svc = Service::new();

    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();

    assert_eq!(svc.window().displayed, Displayed::Session(pending.session().clone()));
    assert!(!svc.session(pending.session()).unwrap().attention);
}

#[test]
fn a_session_opening_while_another_is_displayed_gets_attention_and_does_not_take_the_view() {
    let (a, b) = (agent_branch(), agent_branch());
    let svc = Service::new();
    let first = svc.review(ReviewRequest::new(a.path())).unwrap();

    let second = svc.review(ReviewRequest::new(b.path())).unwrap();

    assert_eq!(svc.window().displayed, Displayed::Session(first.session().clone()));
    assert!(svc.session(second.session()).unwrap().attention);
    assert!(!svc.session(first.session()).unwrap().attention);
}

#[test]
fn displaying_a_session_clears_its_attention() {
    let (a, b) = (agent_branch(), agent_branch());
    let svc = Service::new();
    let _first = svc.review(ReviewRequest::new(a.path())).unwrap();
    let second = svc.review(ReviewRequest::new(b.path())).unwrap();

    svc.display(second.session()).unwrap();

    assert_eq!(svc.window().displayed, Displayed::Session(second.session().clone()));
    assert!(!svc.session(second.session()).unwrap().attention);
}

#[test]
fn a_session_becoming_the_humans_turn_again_while_not_displayed_gets_attention() {
    let (a, b) = (agent_branch(), agent_branch());
    let svc = Service::new();
    let first = svc.review(ReviewRequest::new(a.path())).unwrap();
    let id = first.session().clone();
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 2..=2), "Why b?").unwrap();
    svc.hand_over_round(&id).unwrap();
    let other = svc.review(ReviewRequest::new(b.path())).unwrap();
    svc.display(other.session()).unwrap();
    assert!(!svc.session(&id).unwrap().attention, "the agent's Turn needs no attention");

    let _again = svc.review(ReviewRequest::new(a.path())).unwrap();

    assert!(svc.session(&id).unwrap().attention);
    assert_eq!(svc.window().displayed, Displayed::Session(other.session().clone()), "the view stays put");
}

#[test]
fn the_displayed_session_becoming_the_humans_turn_gets_no_attention() {
    let repo = agent_branch();
    let svc = Service::new();
    let first = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = first.session().clone();
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 2..=2), "Why b?").unwrap();
    svc.hand_over_round(&id).unwrap();

    let _again = svc.review(ReviewRequest::new(repo.path())).unwrap();

    assert!(!svc.session(&id).unwrap().attention);
}

#[test]
fn when_the_displayed_session_ends_the_inbox_is_displayed_and_the_next_session_opening_takes_the_view() {
    let (a, b) = (agent_branch(), agent_branch());
    let svc = Service::new();
    let first = svc.review(ReviewRequest::new(a.path())).unwrap();
    let id = first.session().clone();

    svc.approve(&id).unwrap();
    assert_eq!(svc.window().displayed, Displayed::Inbox);

    let next = svc.review(ReviewRequest::new(b.path())).unwrap();
    assert_eq!(svc.window().displayed, Displayed::Session(next.session().clone()));
    assert!(!svc.session(next.session()).unwrap().attention);
}

#[test]
fn displaying_an_unknown_session_fails() {
    let svc = Service::new();

    let displayed = svc.display(&SessionId::new("nope"));

    assert!(matches!(displayed, Err(Error::UnknownSession(_))), "{displayed:?}");
    assert_eq!(svc.window().displayed, Displayed::Inbox);
}
