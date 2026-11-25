//! UI: folding the General Threads section above the diff (#96), in Rune's headless `Runtime`,
//! against real Sessions in the core service.

use std::sync::Arc;

use rune::accesskit::Action;
use rune::{Env, Runtime};

use akshually_core::{Anchor, ReviewRequest, Service, SessionId, Side, ThreadRef};
use akshually_testkit::TestRepo;
use akshually_ui::{State, view};

/// A repository on a feature branch with one commit ahead of `main`.
fn repo_on_a_feature_branch() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("lib.rs", "fn a() {}\n").commit_all("base");
    repo.branch("feature").write("lib.rs", "fn a() {}\nfn b() {}\n").commit_all("add b");
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

/// Presses the button labelled `label`, which has no key to press it by.
fn press_labelled(rt: &mut Runtime<State>, label: &str) {
    let (x, y) = rt
        .elements()
        .into_iter()
        .find(|e| e.label.as_deref() == Some(label) && !e.exiting)
        .map(|e| (e.rect[0] + e.rect[2] / 2.0, e.rect[1] + e.rect[3] / 2.0))
        .unwrap_or_else(|| panic!("no button labelled `{label}`"));
    rt.pointer_move(x, y);
    rt.frame(0.0);
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(0.0);
}

/// Whether any text drawn in the window contains `needle`.
fn shows(rt: &Runtime<State>, needle: &str) -> bool {
    rt.elements().into_iter().filter(|e| !e.exiting).filter_map(|e| e.text).any(|t| t.contains(needle))
}

/// The texts drawn inside the element keyed `key`, in tree order.
fn texts_within(rt: &Runtime<State>, key: &str) -> Vec<String> {
    let within = format!("/{key}/");
    rt.elements().into_iter().filter(|e| format!("{}/", e.path).contains(&within)).filter_map(|e| e.text).collect()
}

/// A Session over a feature branch where a co-Reviewer left a Draft on the whole Review, and
/// a window showing it.
fn review_with_a_general_draft() -> (TestRepo, Arc<Service>, SessionId, Runtime<State>) {
    let repo = repo_on_a_feature_branch();
    let service = Arc::new(Service::new());
    service.draft(repo.path(), Anchor::Review, "Needs a second look.").unwrap();
    let pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    let rt = window(&service);
    (repo, service, pending.session().clone(), rt)
}

#[test]
fn a_session_opened_with_a_general_thread_shows_the_section_expanded_with_its_count_and_hide() {
    let (_repo, _service, _id, rt) = review_with_a_general_draft();

    assert!(shows(&rt, "Needs a second look."), "the Thread isn't shown");
    assert!(texts_within(&rt, "general-header").iter().any(|t| t == "General Threads · 1"), "no count in the header");
    assert!(texts_within(&rt, "general-header").iter().any(|t| t == "Hide"), "the header doesn't offer Hide");
}

#[test]
fn hiding_the_section_leaves_only_its_header_row_and_showing_brings_the_threads_back() {
    let (_repo, _service, _id, mut rt) = review_with_a_general_draft();

    press(&mut rt, "general-toggle");

    assert!(!shows(&rt, "Needs a second look."), "the Thread still shows");
    assert!(rt.find("general-scroll").is_none(), "the scroll area still shows");
    assert!(texts_within(&rt, "general-header").iter().any(|t| t == "Show"), "the header doesn't offer Show");

    press(&mut rt, "general-toggle");

    assert!(shows(&rt, "Needs a second look."), "the Thread didn't come back");
}

#[test]
fn a_session_with_no_general_threads_and_no_composer_shows_no_header_row() {
    let repo = repo_on_a_feature_branch();
    let service = Arc::new(Service::new());
    let _pending = service.review(ReviewRequest::new(repo.path())).unwrap();

    let rt = window(&service);

    assert!(rt.find("general-header").is_none(), "an empty section shows its header");
}

#[test]
fn opening_the_composer_on_the_review_expands_a_collapsed_section() {
    let (_repo, _service, _id, mut rt) = review_with_a_general_draft();
    press(&mut rt, "general-toggle");

    press_labelled(&mut rt, "Comment on Review");

    assert!(rt.find("composer").is_some(), "the composer isn't shown");
    assert!(shows(&rt, "Needs a second look."), "the Thread isn't shown");
}

#[test]
fn opening_the_composer_on_a_file_expands_a_collapsed_section() {
    let (_repo, _service, _id, mut rt) = review_with_a_general_draft();
    press(&mut rt, "general-toggle");

    press_labelled(&mut rt, "Comment on file");

    assert!(rt.find("composer").is_some(), "the composer isn't shown");
    assert!(shows(&rt, "Needs a second look."), "the Thread isn't shown");
}

#[test]
fn a_new_general_thread_expands_a_collapsed_section() {
    let (repo, service, _id, mut rt) = review_with_a_general_draft();
    press(&mut rt, "general-toggle");
    service.draft(repo.path(), Anchor::file("lib.rs"), "Why this file?").unwrap();

    deliver_window(&mut rt, &service);

    assert!(shows(&rt, "Why this file?"), "the new Thread isn't shown");
    assert!(shows(&rt, "General Threads · 2"), "the header doesn't count both Threads");
}

#[test]
fn a_new_comment_in_a_general_thread_expands_a_collapsed_section() {
    let (repo, service, session, mut rt) = review_with_a_general_draft();
    let thread = service.window().sessions[0].threads[0].id.clone();
    service.reply_in_thread(&ThreadRef::new(&session, &thread), "Why?").unwrap();
    service.hand_over_round(&session).unwrap();
    deliver_window(&mut rt, &service);
    press(&mut rt, "general-toggle");
    service.reply(repo.path(), &thread, "And a second thought.").unwrap();

    deliver_window(&mut rt, &service);

    assert!(shows(&rt, "And a second thought."), "the new Comment isn't shown");
}

#[test]
fn an_update_that_adds_nothing_to_the_general_threads_leaves_a_collapsed_section_collapsed() {
    let (repo, service, _id, mut rt) = review_with_a_general_draft();
    press(&mut rt, "general-toggle");
    service.draft(repo.path(), Anchor::lines(Side::New, "lib.rs", 2..=2), "Name?").unwrap();

    deliver_window(&mut rt, &service);

    assert!(!shows(&rt, "Needs a second look."), "the section expanded");
    assert!(texts_within(&rt, "general-header").iter().any(|t| t == "Show"), "the header doesn't offer Show");
}

#[test]
fn collapsing_the_section_in_one_sessions_tab_leaves_another_sessions_expanded() {
    let (_repo, service, first, mut rt) = review_with_a_general_draft();
    let other = repo_on_a_feature_branch();
    service.draft(other.path(), Anchor::Review, "A note on the other Review.").unwrap();
    let second = service.review(ReviewRequest::new(other.path())).unwrap();
    deliver_window(&mut rt, &service);
    press(&mut rt, &format!("t-{first}"));
    deliver_window(&mut rt, &service);
    press(&mut rt, "general-toggle");

    press(&mut rt, &format!("t-{}", second.session()));
    deliver_window(&mut rt, &service);
    assert!(rt.find(&format!("diff-{}", second.session())).is_some(), "the other tab isn't shown");

    assert!(shows(&rt, "A note on the other Review."), "the other Session's section collapsed too");
}

/// A Session of uncommitted work with a Comment on the whole Review, one on the whole file
/// `a.txt` and one on its line 1, and a window showing it.
fn work_with_three_comments() -> Runtime<State> {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n").commit_all("base");
    repo.write("a.txt", "two\n");
    let service = Arc::new(Service::new());
    let pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    let id = pending.session();
    service.comment(id, Anchor::Review, "Note on the Review").unwrap();
    service.comment(id, Anchor::file("a.txt"), "Note on the file").unwrap();
    service.comment(id, Anchor::lines(Side::New, "a.txt", 1..=1), "Note on the line").unwrap();
    window(&service)
}

#[test]
fn threads_on_the_review_and_on_whole_files_fold_together_and_threads_on_lines_stay() {
    let mut rt = work_with_three_comments();
    assert!(texts_within(&rt, "general-header").iter().any(|t| t == "General Threads · 2"), "the count is off");

    press(&mut rt, "general-toggle");

    assert!(!shows(&rt, "Note on the Review"), "the Review's Thread still shows");
    assert!(!shows(&rt, "Note on the file"), "the whole file's Thread still shows");
    assert!(shows(&rt, "Note on the line"), "the Thread on a line folded with the section");
}

#[test]
fn outdated_threads_are_neither_counted_nor_folded_with_the_section() {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n").commit_all("base");
    repo.write("a.txt", "two\n");
    let service = Arc::new(Service::new());
    let pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    service.comment(pending.session(), Anchor::Review, "Note on the Review").unwrap();
    service.comment(pending.session(), Anchor::file("a.txt"), "Note on the file").unwrap();
    repo.write("a.txt", "one\n");
    service.reload(pending.session()).unwrap();
    let mut rt = window(&service);
    assert!(texts_within(&rt, "general-header").iter().any(|t| t == "General Threads · 1"), "the count is off");

    press(&mut rt, "general-toggle");

    assert!(!shows(&rt, "Note on the Review"), "the Review's Thread still shows");
    assert!(shows(&rt, "Outdated"), "the Outdated Thread folded with the section");
}

#[test]
fn hide_is_not_offered_while_the_composer_is_open_in_the_section() {
    let (_repo, _service, _id, mut rt) = review_with_a_general_draft();

    press_labelled(&mut rt, "Comment on Review");

    assert!(rt.find("composer").is_some(), "the composer isn't shown");
    assert!(rt.find("general-toggle").is_none(), "the header offers a Hide that couldn't do anything");
    assert!(shows(&rt, "Needs a second look."), "the Thread isn't shown");
}
