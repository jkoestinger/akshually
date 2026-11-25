//! UI: the Window's Approve button asking before it discards unsent Drafts and reopened Threads
//! (#104), in Rune's headless `Runtime` against real Sessions in the core service.

use std::sync::Arc;
use std::time::Duration;

use rune::Runtime;
use rune::accesskit::Action;

use akshually_core::{Anchor, Ending, Outcome, ReviewRequest, Service, SessionId, Side, ThreadId, ThreadRef};
use akshually_testkit::TestRepo;
use akshually_ui::State;

mod common;

use common::{repo_with_one_edit, window};

/// Presses the element keyed `key` as an assistive client would.
fn press(rt: &mut Runtime<State>, key: &str) {
    assert!(rt.accessibility_action(key, Action::Click, None), "nothing to press at `{key}`");
    rt.frame(0.0);
}

/// The texts drawn inside the element keyed `key`, in tree order; empty when nothing has the key.
fn texts_within(rt: &Runtime<State>, key: &str) -> Vec<String> {
    let elements = rt.elements();
    let inside = |mut at: usize| loop {
        if elements[at].key.as_deref() == Some(key) {
            return true;
        }
        match elements[at].parent {
            Some(parent) => at = parent,
            None => return false,
        }
    };
    (0..elements.len()).filter(|&at| inside(at)).filter_map(|at| elements[at].text.clone()).collect()
}

/// Local work in the human's Turn with `drafts` unsent Drafts.
fn session_with_drafts(drafts: usize) -> (TestRepo, Arc<Service>, SessionId) {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    for _ in 0..drafts {
        service.comment(pending.session(), Anchor::Review, "Why?").unwrap();
    }
    let id = pending.session().clone();
    (repo, service, id)
}

/// Local work whose Round 1, with Thread T1, the agent has received, in the human's Turn again.
fn session_after_round_1(repo: &TestRepo, service: &Service) -> SessionId {
    let first = service.review(ReviewRequest::new(repo.path())).unwrap();
    let id = first.session().clone();
    service.comment(&id, Anchor::lines(Side::New, "a.txt", 1..=1), "Why?").unwrap();
    service.hand_over_round(&id).unwrap();
    first.wait_timeout(Duration::from_secs(5)).expect("the agent receives Round 1");
    drop(service.review(ReviewRequest::new(repo.path())).unwrap());
    id
}

#[test]
fn approving_with_nothing_unsent_ends_the_session_without_asking() {
    let (repo, service, id) = session_with_drafts(0);
    let mut rt = window(&service);

    press(&mut rt, "approve");

    assert!(rt.find("approve-confirm").is_none(), "a question was asked");
    assert_eq!(service.session(&id).unwrap().ended, Some(Ending::Approved));
    let outcome = service.review(ReviewRequest::new(repo.path())).unwrap().wait_timeout(Duration::from_secs(5));
    assert_eq!(outcome, Some(Outcome::Approved));
}

#[test]
fn approving_with_unsent_drafts_asks_with_their_number_and_does_not_approve() {
    let (_repo, service, id) = session_with_drafts(2);
    let mut rt = window(&service);

    press(&mut rt, "approve");

    let asked = texts_within(&rt, "approve-confirm");
    assert!(asked.iter().any(|t| t.contains("2 unsent items are")), "asked {asked:?}");
    assert!(service.session(&id).unwrap().ended.is_none(), "it ended before the human confirmed");
}

#[test]
fn approving_with_one_unsent_draft_says_item_in_the_singular() {
    let (_repo, service, _id) = session_with_drafts(1);
    let mut rt = window(&service);

    press(&mut rt, "approve");

    let asked = texts_within(&rt, "approve-confirm");
    assert!(asked.iter().any(|t| t.contains("1 unsent item is")), "asked {asked:?}");
}

#[test]
fn approving_with_only_a_reopened_thread_asks() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let id = session_after_round_1(&repo, &service);
    service.resolve(repo.path(), &ThreadId::new("T1")).unwrap();
    service.reopen_thread(&ThreadRef::new(&id, &ThreadId::new("T1"))).unwrap();
    let mut rt = window(&service);

    press(&mut rt, "approve");

    let asked = texts_within(&rt, "approve-confirm");
    assert!(asked.iter().any(|t| t.contains("1 unsent item is")), "asked {asked:?}");
    assert!(service.session(&id).unwrap().ended.is_none(), "it ended before the human confirmed");
}

#[test]
fn approving_with_open_threads_and_nothing_unsent_does_not_ask() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let id = session_after_round_1(&repo, &service);
    let mut rt = window(&service);

    press(&mut rt, "approve");

    assert!(rt.find("approve-confirm").is_none(), "a question was asked");
    assert_eq!(service.session(&id).unwrap().ended, Some(Ending::Approved));
}

#[test]
fn cancelling_the_question_keeps_the_session_and_its_drafts_to_hand_over() {
    let (_repo, service, id) = session_with_drafts(2);
    let mut rt = window(&service);
    press(&mut rt, "approve");

    press(&mut rt, "approve-confirm-cancel");

    assert!(rt.find("approve-confirm").is_none(), "the question is still asked");
    let session = service.session(&id).unwrap();
    assert!(session.ended.is_none(), "the Session ended");
    assert_eq!(session.to_hand_over(), 2);
    assert!(session.actions().hand_over.is_some_and(|h| h.offered), "the Round can't be handed over");
}

#[test]
fn confirming_the_question_approves_and_the_agent_receives_only_the_verdict() {
    let (repo, service, id) = session_with_drafts(2);
    let mut rt = window(&service);
    press(&mut rt, "approve");

    press(&mut rt, "approve-confirm-approve");

    assert_eq!(service.session(&id).unwrap().ended, Some(Ending::Approved));
    let outcome = service.review(ReviewRequest::new(repo.path())).unwrap().wait_timeout(Duration::from_secs(5));
    assert_eq!(outcome, Some(Outcome::Approved));
}

/// Hands the service's Window to the window, as its subscriber does, and builds the next frame.
fn deliver_window(rt: &mut Runtime<State>, service: &Service) {
    let window = service.window();
    rt.update_if(move |s: &mut State, _| {
        s.apply(window);
        true
    });
    rt.frame(0.0);
}

#[test]
fn a_question_left_unanswered_while_the_round_went_over_is_not_asked_again_in_the_next_round() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let first = service.review(ReviewRequest::new(repo.path())).unwrap();
    let id = first.session().clone();
    service.comment(&id, Anchor::Review, "Why?").unwrap();
    let mut rt = window(&service);
    press(&mut rt, "approve");
    assert!(rt.find("approve-confirm").is_some(), "the question wasn't asked");
    service.hand_over_round(&id).unwrap();
    first.wait_timeout(Duration::from_secs(5)).expect("the agent receives Round 1");
    drop(service.review(ReviewRequest::new(repo.path())).unwrap());
    deliver_window(&mut rt, &service);
    assert!(rt.find("approve-confirm").is_none(), "the question stayed after the Round went over");

    service.comment(&id, Anchor::Review, "And this?").unwrap();
    deliver_window(&mut rt, &service);

    assert!(rt.find("approve-confirm").is_none(), "the question came back without Approve being pressed");
}
