//! Seam A: the window-wide diff layout in the core review service, saved in the state directory.

use std::fs;

use akshually_core::{DiffLayout, Service};
use akshually_testkit::TestDirs;

#[test]
fn a_fresh_state_directory_shows_diffs_stacked() {
    let dirs = TestDirs::new();

    let svc = Service::open(dirs.state()).unwrap();

    assert_eq!(svc.diff_layout(), DiffLayout::Stacked);
}

#[test]
fn the_diff_layout_survives_a_restart() {
    let dirs = TestDirs::new();
    let svc = Service::open(dirs.state()).unwrap();
    svc.set_diff_layout(DiffLayout::SideBySide);
    drop(svc);

    let restored = Service::open(dirs.state()).unwrap();

    assert_eq!(restored.diff_layout(), DiffLayout::SideBySide);
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

    assert_eq!(restored.diff_layout(), DiffLayout::Stacked);
}
