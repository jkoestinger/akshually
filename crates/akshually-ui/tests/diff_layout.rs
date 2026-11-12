//! UI: the window-wide diff layout in Rune's headless `Runtime`, against real Sessions in the
//! core service, for what the render smoke check can't reach: its scenes have fixed sample tabs
//! over an empty service, so no Session arrives, reloads or opens in a layout already saved.

use std::sync::Arc;

use rune::accesskit::Action;
use rune::{Env, Runtime};
use serde_json::Value;

use akshually_core::{DiffLayout, ReviewRequest, Service, SessionId};
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

/// Hands the service's Sessions to the window, as its subscriber does, and rebuilds the view.
fn deliver_sessions(rt: &mut Runtime<State>, service: &Service) {
    let sessions = service.sessions();
    rt.update_if(move |s: &mut State, _| {
        s.apply(sessions);
        true
    });
    rt.frame(0.0);
}

/// Presses the element keyed `key` as an assistive client would: the same handlers a click
/// reaches, without waiting for the tab strip to settle where a new tab slides in.
fn press(rt: &mut Runtime<State>, key: &str) {
    assert!(rt.accessibility_action(key, Action::Click, None), "nothing to press at `{key}`");
    rt.frame(0.0);
}

/// Picks the side-by-side layout in the shown tab's header.
fn pick_side_by_side(rt: &mut Runtime<State>) {
    press(rt, "segment1");
}

/// Opens the tab of Session `id`.
fn open_tab(rt: &mut Runtime<State>, id: &SessionId) {
    press(rt, &format!("t-{id}"));
    assert!(rt.find(&format!("diff-{id}")).is_some(), "the tab of {id} didn't open");
}

/// How the shown tab lays out its diff: side by side when its removed line sits level with
/// its added one, which stacked puts on the next row.
fn shown_layout(rt: &Runtime<State>) -> DiffLayout {
    fn top(node: &Value, kind: &str) -> Option<f64> {
        if node["role"] == "list-item" && node["name"].as_str().is_some_and(|n| n.starts_with(kind)) {
            return node["bounds"][1].as_f64();
        }
        node["children"].as_array().into_iter().flatten().find_map(|child| top(child, kind))
    }
    let tree = rt.accessibility_json();
    let line = |kind: &str| top(&tree, kind).unwrap_or_else(|| panic!("the diff shows no {kind}"));
    if line("removed line") == line("added line") { DiffLayout::SideBySide } else { DiffLayout::Stacked }
}

#[test]
fn a_session_arriving_after_the_switch_opens_in_the_current_layout() {
    let (repo_a, repo_b) = (repo_with_one_edit(), repo_with_one_edit());
    let service = Arc::new(Service::new());
    let _pending_a = service.review(ReviewRequest::new(repo_a.path())).unwrap();
    let mut rt = window(&service);
    pick_side_by_side(&mut rt);

    let pending_b = service.review(ReviewRequest::new(repo_b.path())).unwrap();
    deliver_sessions(&mut rt, &service);
    open_tab(&mut rt, pending_b.session());

    assert_eq!(shown_layout(&rt), DiffLayout::SideBySide);
    assert_eq!(service.diff_layout(), DiffLayout::SideBySide, "the service keeps the layout");
}

#[test]
fn reloading_a_session_keeps_the_current_layout() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    let mut rt = window(&service);
    pick_side_by_side(&mut rt);

    repo.write("a.txt", "three\n");
    service.reload(pending.session()).unwrap();
    deliver_sessions(&mut rt, &service);

    assert_eq!(shown_layout(&rt), DiffLayout::SideBySide);
}

#[test]
fn restored_tabs_open_in_the_saved_layout() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let _pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    service.set_diff_layout(DiffLayout::SideBySide);

    let rt = window(&service);

    assert_eq!(shown_layout(&rt), DiffLayout::SideBySide);
}
