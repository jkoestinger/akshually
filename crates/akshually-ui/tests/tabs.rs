//! UI: picking what the Window displays, in Rune's headless `Runtime` against real Sessions in
//! the core service: a pick shows once the service took it, and one it refused changes nothing.

use std::sync::Arc;

use rune::accesskit::Action;
use rune::{Env, Runtime};

use akshually_core::{ReviewRequest, Service};
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

/// Presses the element keyed `key` as an assistive client would.
fn press(rt: &mut Runtime<State>, key: &str) {
    assert!(rt.accessibility_action(key, Action::Click, None), "nothing to press at `{key}`");
    rt.frame(0.0);
}

#[test]
fn picking_a_tab_shows_its_session() {
    let (repo_a, repo_b) = (repo_with_one_edit(), repo_with_one_edit());
    let service = Arc::new(Service::new());
    let _a = service.review(ReviewRequest::new(repo_a.path())).unwrap();
    let b = service.review(ReviewRequest::new(repo_b.path())).unwrap();
    let mut rt = window(&service);

    press(&mut rt, &format!("t-{}", b.session()));

    assert!(rt.find(&format!("diff-{}", b.session())).is_some(), "the picked tab's Session doesn't show");
}

#[test]
fn picking_the_tab_of_a_session_that_just_ended_leaves_the_view_as_it_was() {
    let (repo_a, repo_b) = (repo_with_one_edit(), repo_with_one_edit());
    let service = Arc::new(Service::new());
    let shown = service.review(ReviewRequest::new(repo_a.path())).unwrap();
    let ended = service.review(ReviewRequest::new(repo_b.path())).unwrap();
    let mut rt = window(&service);
    // Ended, but the window hasn't had the update: its tab is still there to pick.
    service.approve(ended.session()).unwrap();

    press(&mut rt, &format!("t-{}", ended.session()));

    assert!(rt.find(&format!("diff-{}", shown.session())).is_some(), "the view moved off the Session it showed");
    assert!(rt.find(&format!("diff-{}", ended.session())).is_none(), "the ended Session shows");
}
