//! UI: a Session's Changeset view in Rune's headless `Runtime`, against real Sessions in the
//! core service, for what the render smoke check can't reach: input that lands between an
//! update and the next rebuild (#37).

use std::sync::Arc;

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

/// Hands the service's Sessions to the window, as its subscriber does: the tabs change at once,
/// and the view rebuilds on the next frame.
fn deliver_sessions(rt: &mut Runtime<State>, service: &Service) {
    let sessions = service.sessions();
    rt.update_if(move |s: &mut State, _| {
        s.apply(sessions);
        true
    });
}

#[test]
fn hovering_the_changeset_of_a_session_that_just_ended_drops_it_without_panicking() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    let mut rt = window(&service);
    let (x, y) = rt.find("code").expect("the Session's Changeset shows");

    service.approve(pending.session()).unwrap();
    deliver_sessions(&mut rt, &service);
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
    deliver_sessions(&mut rt, &service);
    service.display(next).unwrap();
    deliver_sessions(&mut rt, &service);
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(0.0);

    assert!(rt.find("composer").is_none(), "the ended Session's \"+\" opened a composer on another Session");
}
