//! UI: the window-wide diff layout in Rune's headless `Runtime`, against real Sessions in the
//! core service: the Session header's control switches every tab, and tabs opened later take
//! the current layout.

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

/// How the shown tab lays out its diff, read from where its one changed line sits: the removed
/// line beside the added one (`"side by side"`), or above it (`"stacked"`).
fn shown_layout(rt: &Runtime<State>) -> &'static str {
    fn find<'a>(node: &'a Value, name: &str) -> Option<&'a Value> {
        if node["role"] == "list-item" && node["name"] == name {
            return Some(node);
        }
        node["children"].as_array().into_iter().flatten().find_map(|child| find(child, name))
    }
    let tree = rt.accessibility_json();
    let top = |name: &str| {
        let row = find(&tree, name).unwrap_or_else(|| panic!("the diff shows no \"{name}\""));
        row["bounds"][1].as_f64().unwrap()
    };
    let (removed, added) = (top("removed line 1: one"), top("added line 1: two"));
    if removed == added {
        "side by side"
    } else {
        assert!(removed < added, "the removed line shows below the added one");
        "stacked"
    }
}

#[test]
fn a_tab_opens_stacked_by_default() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let _pending = service.review(ReviewRequest::new(repo.path())).unwrap();

    let rt = window(&service);

    assert_eq!(shown_layout(&rt), "stacked");
}

#[test]
fn switching_to_side_by_side_on_one_tab_switches_every_tab() {
    let (repo_a, repo_b) = (repo_with_one_edit(), repo_with_one_edit());
    let service = Arc::new(Service::new());
    let pending_a = service.review(ReviewRequest::new(repo_a.path())).unwrap();
    let pending_b = service.review(ReviewRequest::new(repo_b.path())).unwrap();
    service.display(pending_a.session()).unwrap();
    let mut rt = window(&service);

    pick_side_by_side(&mut rt);
    open_tab(&mut rt, pending_b.session());

    assert_eq!(shown_layout(&rt), "side by side");
    assert_eq!(service.diff_layout(), DiffLayout::SideBySide, "the service keeps the layout");
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

    assert_eq!(shown_layout(&rt), "side by side");
}

#[test]
fn restored_tabs_open_in_the_saved_layout() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let _pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    service.set_diff_layout(DiffLayout::SideBySide);

    let rt = window(&service);

    assert_eq!(shown_layout(&rt), "side by side");
}
