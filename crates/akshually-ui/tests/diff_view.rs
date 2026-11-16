//! UI: a Session's Changeset view in Rune's headless `Runtime`, against real Sessions in the
//! core service, for what the render smoke check can't reach: input that lands between an
//! update and the next rebuild (#37), the comment box across updates (#48), and expanding a
//! Resolved Thread (#69).

use std::sync::Arc;
use std::time::Duration;

use rune::accesskit::Action;
use rune::{Env, Runtime};

use akshually_core::{Anchor, ReviewRequest, Service, SessionId, Side, ThreadId, ThreadRef};
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

/// Hands the service's Window to the window, as its subscriber does: the tabs change at once,
/// and the view rebuilds on the next frame.
fn deliver_window(rt: &mut Runtime<State>, service: &Service) {
    let window = service.window();
    rt.update_if(move |s: &mut State, _| {
        s.apply(window);
        true
    });
}

/// Hands the service's Window to the window and builds the next frame from it.
fn deliver_window_and_rebuild(rt: &mut Runtime<State>, service: &Service) {
    deliver_window(rt, service);
    rt.frame(0.0);
}

/// Opens the comment box on the first line the shown Changeset offers a Comment on.
fn open_comment_box(rt: &mut Runtime<State>) {
    let (x, y) = rt.find("plus").expect("the Session's Changeset offers a Comment on its lines");
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(0.0);
    assert!(rt.find("composer").is_some(), "the comment box didn't open");
}

/// The Session of `repo`'s first Round: the human's Comment `T1` on its one line, which the
/// agent answered and resolved.
fn resolved_by_the_agent(repo: &TestRepo, service: &Service) -> SessionId {
    let pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    let id = pending.session().clone();
    service.comment(&id, Anchor::lines(Side::New, "a.txt", 1..=1), "Why two?").unwrap();
    service.hand_over_round(&id).unwrap();
    assert!(pending.wait_timeout(Duration::from_secs(5)).is_some(), "the Round wasn't handed over");
    let t1 = ThreadId::new("T1");
    service.reply(repo.path(), &t1, "Because one was wrong.").unwrap();
    service.resolve(repo.path(), &t1).unwrap();
    id
}

/// Presses the element keyed `key` as an assistive client would.
fn press(rt: &mut Runtime<State>, key: &str) {
    assert!(rt.accessibility_action(key, Action::Click, None), "nothing to press at `{key}`");
    rt.frame(0.0);
}

#[test]
fn a_thread_the_agent_resolved_in_a_round_arrives_collapsed() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let _id = resolved_by_the_agent(&repo, &service);

    let rt = window(&service);

    assert!(rt.find("resolved-T1").is_some(), "the Resolved Thread doesn't show as one line");
    assert!(rt.find("resolved-comments-T1").is_none(), "the Resolved Thread arrived expanded");
}

#[test]
fn showing_a_resolved_thread_reveals_its_comments_without_a_reply_box() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let _id = resolved_by_the_agent(&repo, &service);
    let mut rt = window(&service);

    press(&mut rt, "toggle-T1");

    assert!(rt.find("resolved-comments-T1").is_some(), "Show didn't reveal the Thread's Comments");
    assert!(rt.find("reply-T1").is_none(), "the expanded Resolved Thread offers a Reply");
    assert!(rt.find("reopen-T1").is_some(), "Reopen left the summary line");
}

#[test]
fn hiding_an_expanded_resolved_thread_collapses_it_to_one_line_again() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let _id = resolved_by_the_agent(&repo, &service);
    let mut rt = window(&service);
    press(&mut rt, "toggle-T1");

    press(&mut rt, "toggle-T1");

    assert!(rt.find("resolved-T1").is_some(), "the Resolved Thread's line went");
    assert!(rt.find("resolved-comments-T1").is_none(), "Hide left the Thread's Comments showing");
}

#[test]
fn reopening_a_resolved_thread_offers_a_reply_again() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let _id = resolved_by_the_agent(&repo, &service);
    let mut rt = window(&service);

    press(&mut rt, "reopen-T1");
    deliver_window_and_rebuild(&mut rt, &service);

    assert!(rt.find("reply-T1").is_some(), "the reopened Thread doesn't offer a Reply");
}

#[test]
fn an_expanded_thread_reopened_and_resolved_again_comes_back_collapsed() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let id = resolved_by_the_agent(&repo, &service);
    let mut rt = window(&service);
    press(&mut rt, "toggle-T1");

    press(&mut rt, "reopen-T1");
    deliver_window_and_rebuild(&mut rt, &service);
    service.resolve_thread(&ThreadRef::new(&id, &ThreadId::new("T1"))).unwrap();
    deliver_window_and_rebuild(&mut rt, &service);

    assert!(rt.find("resolved-T1").is_some(), "the Thread resolved again doesn't show as one line");
    assert!(rt.find("resolved-comments-T1").is_none(), "the Thread resolved again came back expanded");
}

#[test]
fn an_expanded_outdated_thread_stays_expanded_through_an_update() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    service.comment(pending.session(), Anchor::lines(Side::New, "a.txt", 1..=1), "Why two?").unwrap();
    repo.write("a.txt", "three\n");
    service.reload(pending.session()).unwrap();
    let mut rt = window(&service);
    press(&mut rt, "toggle-T1");
    assert!(rt.find("outdated-comments-T1").is_some(), "Show didn't expand the Outdated Thread");

    service.mark_viewed(pending.session(), "a.txt", true).unwrap();
    deliver_window_and_rebuild(&mut rt, &service);

    assert!(rt.find("outdated-comments-T1").is_some(), "the update collapsed the expanded Outdated Thread");
}

#[test]
fn hovering_the_changeset_of_a_session_that_just_ended_drops_it_without_panicking() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    let mut rt = window(&service);
    let (x, y) = rt.find("code").expect("the Session's Changeset shows");

    service.approve(pending.session()).unwrap();
    deliver_window(&mut rt, &service);
    rt.pointer_move(x, y);
    rt.frame(0.0);

    assert!(rt.find("code").is_none(), "the ended Session's Changeset still shows");
}

#[test]
fn commenting_on_a_line_of_a_session_that_just_ended_opens_no_composer_on_the_session_shown_next() {
    let (repo_a, repo_b) = (repo_with_one_edit(), repo_with_one_edit());
    let service = Arc::new(Service::new());
    let pending_ended = service.review(ReviewRequest::new(repo_a.path())).unwrap();
    let pending_next = service.review(ReviewRequest::new(repo_b.path())).unwrap();
    let (ended, next) = (pending_ended.session(), pending_next.session());
    service.display(ended).unwrap();
    let mut rt = window(&service);
    let (x, y) = rt.find("plus").expect("the Session's Changeset offers a Comment on its lines");

    service.approve(ended).unwrap();
    deliver_window(&mut rt, &service);
    service.display(next).unwrap();
    deliver_window(&mut rt, &service);
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(0.0);

    assert!(rt.find("composer").is_none(), "the ended Session's \"+\" opened a composer on another Session");
}

#[test]
fn the_open_comment_box_stays_through_an_update_that_leaves_the_changeset_shown_alone() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    let mut rt = window(&service);
    open_comment_box(&mut rt);

    service.mark_viewed(pending.session(), "a.txt", true).unwrap();
    deliver_window(&mut rt, &service);
    rt.frame(0.0);

    assert!(rt.find("composer").is_some(), "an update that left the Changeset shown alone dropped the comment box");
}

#[test]
fn the_open_comment_box_goes_when_the_changeset_shown_changes() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    let mut rt = window(&service);
    open_comment_box(&mut rt);

    repo.write("a.txt", "three\n");
    service.reload(pending.session()).unwrap();
    deliver_window(&mut rt, &service);
    rt.frame(0.0);

    assert!(rt.find("composer").is_none(), "the comment box stayed on lines that may be gone");
}
