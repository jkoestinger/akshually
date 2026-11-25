//! UI: the Window's Edit, Save, Cancel and Discard on the human's own Drafts in Rune's headless
//! `Runtime`, against real Sessions in the core service (#108).

use std::sync::Arc;
use std::time::Duration;

use rune::Runtime;
use rune::accesskit::Action;

use akshually_core::{Anchor, ReviewRequest, Service, SessionId, Side, ThreadId, ThreadRef};
use akshually_ui::State;

mod common;

use common::{repo_with_one_edit, window};

/// Hands the service's Window to the window, as its subscriber does, and builds the next frame.
fn deliver_window(rt: &mut Runtime<State>, service: &Service) {
    let window = service.window();
    rt.update_if(move |s: &mut State, _| {
        s.apply(window);
        true
    });
    rt.frame(0.0);
}

/// Presses the element keyed `key` as an assistive client would, then hands the window what the
/// service did.
fn press(rt: &mut Runtime<State>, service: &Service, key: &str) {
    assert!(rt.accessibility_action(key, Action::Click, None), "nothing to press at `{key}`");
    rt.frame(0.0);
    deliver_window(rt, service);
}

/// Every text the window draws.
fn texts(rt: &Runtime<State>) -> Vec<String> {
    rt.elements().into_iter().filter(|e| !e.exiting).filter_map(|e| e.text).collect()
}

fn shows(rt: &Runtime<State>, text: &str) -> bool {
    texts(rt).iter().any(|t| t.contains(text))
}

/// The Session of `repo` with the human's Comment `T1` on its one line, still a Draft.
fn draft(service: &Service, repo: &akshually_testkit::TestRepo) -> SessionId {
    let id = service.review(ReviewRequest::new(repo.path())).unwrap().session().clone();
    service.comment(&id, Anchor::lines(Side::New, "a.txt", 1..=1), "Why two?").unwrap();
    id
}

/// `draft`'s Thread, once the agent received it and answered, with the human's Draft reply.
fn reply_draft(service: &Service, repo: &akshually_testkit::TestRepo) -> SessionId {
    let pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    let id = pending.session().clone();
    service.comment(&id, Anchor::lines(Side::New, "a.txt", 1..=1), "Why two?").unwrap();
    service.hand_over_round(&id).unwrap();
    assert!(pending.wait_timeout(Duration::from_secs(5)).is_some(), "the Round wasn't handed over");
    let t1 = ThreadId::new("T1");
    service.reply(repo.path(), &t1, "Because one was wrong.").unwrap();
    service.reply_in_thread(&ThreadRef::new(&id, &t1), "Thanks, but why?").unwrap();
    id
}

fn body(service: &Service, id: &SessionId, comment: usize) -> String {
    service.session(id).unwrap().threads[0].comments[comment].body.clone()
}

#[test]
fn edit_opens_an_editor_on_the_drafts_text_and_save_shows_the_new_text_in_the_card() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let id = draft(&service, &repo);
    let mut rt = window(&service);

    press(&mut rt, &service, "edit-T1-0");
    assert!(rt.find("edit-composer").is_some(), "the editor didn't open");
    assert!(shows(&rt, "Why two?"), "the editor doesn't hold the Draft's text");
    rt.paste(" Really?");
    rt.frame(0.0);
    press(&mut rt, &service, "save-edit");

    assert!(rt.find("edit-composer").is_none(), "the editor stayed open");
    assert!(body(&service, &id, 0).contains("Really?"), "{}", body(&service, &id, 0));
    assert!(shows(&rt, "Really?"), "the card doesn't show the new text");
    assert!(service.session(&id).unwrap().threads[0].comments[0].is_draft());
}

#[test]
fn cancel_leaves_the_draft_as_it_was() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let id = draft(&service, &repo);
    let mut rt = window(&service);
    press(&mut rt, &service, "edit-T1-0");
    rt.paste(" Really?");
    rt.frame(0.0);

    press(&mut rt, &service, "cancel-edit");

    assert!(rt.find("edit-composer").is_none(), "the editor stayed open");
    assert_eq!(body(&service, &id, 0), "Why two?");
    assert!(!shows(&rt, "Really?"), "the card shows what was cancelled");
}

#[test]
fn discard_removes_the_thread_that_the_draft_opened() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let id = draft(&service, &repo);
    let mut rt = window(&service);

    press(&mut rt, &service, "discard-T1-0");

    assert!(service.session(&id).unwrap().threads.is_empty(), "the Thread is still there");
    assert!(!shows(&rt, "Why two?"), "the card is still shown");
}

#[test]
fn discard_removes_a_draft_reply_and_leaves_the_rest_of_the_thread() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let id = reply_draft(&service, &repo);
    let mut rt = window(&service);
    assert!(shows(&rt, "Thanks, but why?"), "the Draft reply isn't shown");
    assert!(rt.find("edit-T1-0").is_none(), "the Comment a Round carried offers Edit");

    press(&mut rt, &service, "discard-T1-2");

    assert_eq!(service.session(&id).unwrap().threads[0].comments.len(), 2);
    assert!(!shows(&rt, "Thanks, but why?"), "the reply is still shown");
    assert!(shows(&rt, "Because one was wrong."), "the agent's reply went too");
}

#[test]
fn a_draft_reply_is_edited_in_place_under_the_comments_before_it() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let id = reply_draft(&service, &repo);
    let mut rt = window(&service);

    press(&mut rt, &service, "edit-T1-2");
    rt.paste(" Please.");
    rt.frame(0.0);
    press(&mut rt, &service, "save-edit");

    assert!(body(&service, &id, 2).contains("Please."), "{}", body(&service, &id, 2));
    assert_eq!(body(&service, &id, 1), "Because one was wrong.");
    assert!(shows(&rt, "Please."));
}
