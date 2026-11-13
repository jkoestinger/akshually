//! Seam A: the Window in the core review service: what it displays, its Diff layout (saved in
//! the state directory), and the one subscription that tells the UI all of it.

use std::fs;
use std::sync::{Arc, Mutex};

use akshually_core::{DiffLayout, Displayed, ReviewRequest, Service, WindowView};
use akshually_testkit::{TestDirs, TestRepo};

/// A repository with one uncommitted edit, so its Session's Changeset has one line to show.
fn repo_with_one_edit() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n").commit_all("base");
    repo.write("a.txt", "two\n");
    repo
}

/// Every Window the service tells its Window subscribers of, in order.
fn heard(svc: &Service) -> Arc<Mutex<Vec<WindowView>>> {
    let heard = Arc::new(Mutex::new(Vec::new()));
    let sink = heard.clone();
    svc.subscribe_window(move |window| sink.lock().unwrap().push(window.clone()));
    heard
}

#[test]
fn the_window_displays_the_inbox_at_startup() {
    let dirs = TestDirs::new();

    let svc = Service::open(dirs.state()).unwrap();

    assert_eq!(svc.window().displayed, Displayed::Inbox);
}

#[test]
fn when_the_displayed_session_goes_away_the_window_displays_the_inbox() {
    let (a, b) = (repo_with_one_edit(), repo_with_one_edit());
    let svc = Service::new();
    let first = svc.review(ReviewRequest::new(a.path())).unwrap();
    let _second = svc.review(ReviewRequest::new(b.path())).unwrap();
    assert_eq!(svc.window().displayed, Displayed::Session(first.session().clone()));

    svc.approve(first.session()).unwrap();

    assert_eq!(svc.window().displayed, Displayed::Inbox, "not the other Session");
}

#[test]
fn a_session_going_away_that_the_window_does_not_display_leaves_the_display_put() {
    let (a, b) = (repo_with_one_edit(), repo_with_one_edit());
    let svc = Service::new();
    let first = svc.review(ReviewRequest::new(a.path())).unwrap();
    let second = svc.review(ReviewRequest::new(b.path())).unwrap();

    svc.approve(second.session()).unwrap();

    assert_eq!(svc.window().displayed, Displayed::Session(first.session().clone()));
}

#[test]
fn setting_the_diff_layout_tells_window_subscribers() {
    let svc = Service::new();
    let heard = heard(&svc);

    svc.set_diff_layout(DiffLayout::SideBySide);

    let layouts: Vec<_> = heard.lock().unwrap().iter().map(|w| w.diff_layout).collect();
    assert_eq!(layouts, vec![DiffLayout::SideBySide]);
}

#[test]
fn window_subscribers_see_sessions_open_and_end() {
    let repo = repo_with_one_edit();
    let svc = Service::new();
    let heard = heard(&svc);

    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    svc.approve(pending.session()).unwrap();

    let seen: Vec<_> = heard.lock().unwrap().iter().map(|w| (w.sessions.len(), w.displayed.clone())).collect();
    assert_eq!(seen, vec![(1, Displayed::Session(pending.session().clone())), (0, Displayed::Inbox)]);
}

#[test]
fn displaying_the_inbox_tells_window_subscribers() {
    let repo = repo_with_one_edit();
    let svc = Service::new();
    let _pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let heard = heard(&svc);

    svc.display_inbox();

    let displayed: Vec<_> = heard.lock().unwrap().iter().map(|w| w.displayed.clone()).collect();
    assert_eq!(displayed, vec![Displayed::Inbox]);
}

#[test]
fn a_fresh_state_directory_shows_diffs_stacked() {
    let dirs = TestDirs::new();

    let svc = Service::open(dirs.state()).unwrap();

    assert_eq!(svc.window().diff_layout, DiffLayout::Stacked);
}

#[test]
fn the_diff_layout_survives_a_restart() {
    let dirs = TestDirs::new();
    let svc = Service::open(dirs.state()).unwrap();
    svc.set_diff_layout(DiffLayout::SideBySide);
    drop(svc);

    let restored = Service::open(dirs.state()).unwrap();

    assert_eq!(restored.window().diff_layout, DiffLayout::SideBySide);
}

#[test]
fn an_unreadable_diff_layout_shows_diffs_stacked_without_stopping_the_app() {
    let dirs = TestDirs::new();
    let svc = Service::open(dirs.state()).unwrap();
    svc.set_diff_layout(DiffLayout::SideBySide);
    drop(svc);
    // Whichever file holds the layout, it's now unreadable.
    for entry in fs::read_dir(dirs.state()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "json") {
            fs::write(path, "{ not json").unwrap();
        }
    }

    let restored = Service::open(dirs.state()).expect("an unreadable diff layout doesn't stop the app");

    assert_eq!(restored.window().diff_layout, DiffLayout::Stacked);
}
