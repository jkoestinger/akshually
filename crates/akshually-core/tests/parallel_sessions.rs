//! Seam A, the core review service: parallel Sessions, Turn and attention.

use std::time::Duration;

use akshually_core::{Anchor, Outcome, ReviewRequest, Service, SessionId, Side, Turn};
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

    assert_eq!(svc.displayed().as_ref(), Some(pending.session()));
    assert!(!svc.session(pending.session()).unwrap().attention);
}

#[test]
fn a_session_opening_while_another_is_displayed_gets_attention_and_does_not_take_the_view() {
    let (a, b) = (agent_branch(), agent_branch());
    let svc = Service::new();
    let first = svc.review(ReviewRequest::new(a.path())).unwrap();

    let second = svc.review(ReviewRequest::new(b.path())).unwrap();

    assert_eq!(svc.displayed().as_ref(), Some(first.session()));
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

    assert_eq!(svc.displayed().as_ref(), Some(second.session()));
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
    assert_eq!(svc.displayed().as_ref(), Some(other.session()), "the view stays put");
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
fn when_the_displayed_session_ends_none_is_displayed_and_the_next_one_opening_is() {
    let (a, b) = (agent_branch(), agent_branch());
    let svc = Service::new();
    let first = svc.review(ReviewRequest::new(a.path())).unwrap();
    let id = first.session().clone();

    svc.approve(&id).unwrap();
    assert_eq!(svc.displayed(), None);

    let next = svc.review(ReviewRequest::new(b.path())).unwrap();
    assert_eq!(svc.displayed().as_ref(), Some(next.session()));
    assert!(!svc.session(next.session()).unwrap().attention);
}

#[test]
fn displaying_an_unknown_session_fails() {
    let svc = Service::new();

    assert!(svc.display(&SessionId::new("nope")).is_err());
}

#[test]
fn a_session_is_the_humans_turn_until_a_round_is_handed_over_then_the_agents_until_it_calls_again() {
    let repo = agent_branch();
    let svc = Service::new();
    let first = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = first.session().clone();
    assert_eq!(svc.session(&id).unwrap().turn, Turn::Human);

    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 2..=2), "Why b?").unwrap();
    svc.hand_over_round(&id).unwrap();
    assert_eq!(svc.session(&id).unwrap().turn, Turn::Agent);

    let _second = svc.review(ReviewRequest::new(repo.path())).unwrap();
    assert_eq!(svc.session(&id).unwrap().turn, Turn::Human);
}

#[test]
fn a_review_that_picks_up_a_queued_round_leaves_the_turn_with_the_agent() {
    let repo = agent_branch();
    let svc = Service::new();
    let first = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = first.session().clone();
    drop(first);
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 2..=2), "Why b?").unwrap();
    svc.hand_over_round(&id).unwrap();

    let second = svc.review(ReviewRequest::new(repo.path())).unwrap();

    assert!(matches!(second.wait_timeout(Duration::from_secs(1)), Some(Outcome::Round(_))));
    assert_eq!(svc.session(&id).unwrap().turn, Turn::Agent);
}
