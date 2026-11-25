//! UI: Escape in the text area of the composer of a new Thread or of a reply, in Rune's headless
//! `Runtime` against real Sessions in the core service (#100): it closes the composer when its
//! text is empty, as Discard does, and never destroys writing.

use std::sync::Arc;

use rune::accesskit::Action;
use rune::{Env, Ime, Key, KeyEvent, Runtime};

use akshually_core::{Anchor, ReviewRequest, Service, SessionId, Side, ThreadId};
use akshually_testkit::TestRepo;
use akshually_ui::{State, view};

/// A repository with one uncommitted edit, so its Session's Changeset has one line to show.
fn repo_with_one_edit() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n").commit_all("base");
    repo.write("a.txt", "two\n");
    repo
}

/// A headless window over `service`, its first frame built.
fn window(service: &Arc<Service>) -> Runtime<State> {
    let mut rt = Runtime::new(State::new(service.clone()), view, (1280.0, 800.0), Env::default());
    rt.frame(0.0);
    rt
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

/// Presses the element keyed `key` as an assistive client would.
fn press(rt: &mut Runtime<State>, key: &str) {
    assert!(rt.accessibility_action(key, Action::Click, None), "nothing to press at `{key}`");
    rt.frame(0.0);
}

/// Presses the element at (`x`, `y`) as a pointer does.
fn click_at(rt: &mut Runtime<State>, (x, y): (f32, f32)) {
    rt.pointer_move(x, y);
    rt.frame(0.0);
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(0.0);
}

/// Presses the button labelled `label`, which has no key to press it by.
fn press_labelled(rt: &mut Runtime<State>, label: &str) {
    let at = rt
        .elements()
        .into_iter()
        .find(|e| e.label.as_deref() == Some(label) && !e.exiting)
        .map(|e| (e.rect[0] + e.rect[2] / 2.0, e.rect[1] + e.rect[3] / 2.0))
        .unwrap_or_else(|| panic!("no button labelled `{label}`"));
    click_at(rt, at);
}

/// Types `text` into the focused field.
fn type_text(rt: &mut Runtime<State>, text: &str) {
    rt.paste(text);
    rt.frame(0.0);
}

fn escape(rt: &mut Runtime<State>) {
    rt.key(KeyEvent { key: Key::Escape, mods: Default::default() });
    rt.frame(0.0);
}

/// Whether the element with keyboard focus lies inside the element keyed `key`.
fn focus_is_within(rt: &Runtime<State>, key: &str) -> bool {
    let within = format!("/{key}/");
    rt.elements().into_iter().any(|e| e.focused && format!("{}/", e.path).contains(&within))
}

/// The texts drawn inside the element keyed `key`, in tree order.
fn texts_within(rt: &Runtime<State>, key: &str) -> Vec<String> {
    let within = format!("/{key}/");
    rt.elements().into_iter().filter(|e| format!("{}/", e.path).contains(&within)).filter_map(|e| e.text).collect()
}

/// A Session over `repo_with_one_edit` with the composer open on its one line, focused.
fn open_composer() -> (TestRepo, Arc<Service>, SessionId, Runtime<State>) {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let id = service.review(ReviewRequest::new(repo.path())).unwrap().session().clone();
    let mut rt = window(&service);
    let plus = rt.find("plus").expect("the Changeset offers a Comment on its lines");
    click_at(&mut rt, plus);
    assert!(focus_is_within(&rt, "composer"), "the composer's text area has no focus");
    (repo, service, id, rt)
}

/// A Session whose reopened Thread `T1` has its reply composer open, focused.
fn open_reply_composer() -> (TestRepo, Arc<Service>, SessionId, Runtime<State>) {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    let id = pending.session().clone();
    service.comment(&id, Anchor::lines(Side::New, "a.txt", 1..=1), "Why two?").unwrap();
    service.hand_over_round(&id).unwrap();
    assert!(pending.wait_timeout(std::time::Duration::from_secs(5)).is_some(), "the Round wasn't handed over");
    let t1 = ThreadId::new("T1");
    service.reply(repo.path(), &t1, "Because one was wrong.").unwrap();
    service.resolve(repo.path(), &t1).unwrap();
    let mut rt = window(&service);
    press(&mut rt, "reopen-T1");
    deliver_window(&mut rt, &service);
    press(&mut rt, "reply-T1");
    assert!(focus_is_within(&rt, "reply-composer"), "the reply composer's text area has no focus");
    (repo, service, id, rt)
}

fn drafts(service: &Service, id: &SessionId) -> usize {
    service.session(id).map(|s| s.drafts()).expect("the Session is open")
}

#[test]
fn escape_in_an_empty_composer_closes_it() {
    let (_repo, service, id, mut rt) = open_composer();

    escape(&mut rt);

    assert!(rt.find("composer").is_none(), "Escape left the empty composer open");
    assert_eq!(drafts(&service, &id), 0, "Escape created a Draft");
}

#[test]
fn escape_in_a_composer_holding_only_whitespace_closes_it() {
    let (_repo, service, id, mut rt) = open_composer();
    type_text(&mut rt, "  \n\t ");

    escape(&mut rt);

    assert!(rt.find("composer").is_none(), "Escape left the whitespace-only composer open");
    assert_eq!(drafts(&service, &id), 0, "Escape created a Draft");
}

#[test]
fn escape_in_a_composer_with_text_leaves_it_open_with_its_text() {
    let (_repo, service, id, mut rt) = open_composer();
    type_text(&mut rt, "keep me");

    escape(&mut rt);

    assert!(rt.find("composer").is_some(), "Escape closed a composer holding text");
    assert!(texts_within(&rt, "composer").iter().any(|t| t.contains("keep me")), "the composer lost its text");
    assert!(!focus_is_within(&rt, "composer"), "focus stayed in the text area");
    assert_eq!(drafts(&service, &id), 0, "Escape created a Draft");
}

#[test]
fn escape_in_a_composer_during_an_input_method_composition_closes_nothing() {
    let (_repo, _service, _id, mut rt) = open_composer();
    rt.ime(Ime::Preedit("ka".into(), None, Vec::new()));
    rt.frame(0.0);

    escape(&mut rt);

    assert!(rt.find("composer").is_some(), "Escape closed the composer mid-composition");
    assert!(focus_is_within(&rt, "composer"), "Escape took focus off the composing text area");
}

#[test]
fn a_composer_closed_by_escape_leaves_the_window_as_discard_does() {
    let (_repo, _service, _id, mut rt) = open_composer();
    escape(&mut rt);
    let after_escape: Vec<_> = rt.elements().into_iter().map(|e| (e.key, e.rect)).collect();
    let plus = rt.find("plus").expect("the Changeset offers a Comment on its lines");
    click_at(&mut rt, plus);

    press_labelled(&mut rt, "Discard");

    let after_discard: Vec<_> = rt.elements().into_iter().map(|e| (e.key, e.rect)).collect();
    assert_eq!(after_escape, after_discard, "Escape and Discard leave the window differently");
}

#[test]
fn escape_in_an_empty_reply_composer_closes_it() {
    let (_repo, service, id, mut rt) = open_reply_composer();
    let before = drafts(&service, &id);

    escape(&mut rt);

    assert!(rt.find("reply-composer").is_none(), "Escape left the empty reply composer open");
    assert_eq!(drafts(&service, &id), before, "Escape changed the Drafts");
}

#[test]
fn escape_in_a_reply_composer_holding_only_whitespace_closes_it() {
    let (_repo, _service, _id, mut rt) = open_reply_composer();
    type_text(&mut rt, " \n ");

    escape(&mut rt);

    assert!(rt.find("reply-composer").is_none(), "Escape left the whitespace-only reply composer open");
}

#[test]
fn escape_in_a_reply_composer_with_text_leaves_it_open_with_its_text() {
    let (_repo, service, id, mut rt) = open_reply_composer();
    let before = drafts(&service, &id);
    type_text(&mut rt, "keep me");

    escape(&mut rt);

    assert!(rt.find("reply-composer").is_some(), "Escape closed a reply composer holding text");
    assert!(texts_within(&rt, "reply-composer").iter().any(|t| t.contains("keep me")), "the reply lost its text");
    assert!(!focus_is_within(&rt, "reply-composer"), "focus stayed in the text area");
    assert_eq!(drafts(&service, &id), before, "Escape changed the Drafts");
}

#[test]
fn escape_in_a_reply_composer_during_an_input_method_composition_closes_nothing() {
    let (_repo, _service, _id, mut rt) = open_reply_composer();
    rt.ime(Ime::Preedit("ka".into(), None, Vec::new()));
    rt.frame(0.0);

    escape(&mut rt);

    assert!(rt.find("reply-composer").is_some(), "Escape closed the reply composer mid-composition");
}
