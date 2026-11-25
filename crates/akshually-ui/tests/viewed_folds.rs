//! UI: how a file's Viewed tick folds its diff in a Session's Changeset view (#78), in Rune's
//! headless `Runtime`, against real Sessions in the core service.

use std::sync::Arc;
use std::time::Duration;

use rune::Runtime;
use rune::accesskit::Action;

use akshually_core::{Anchor, ChangesetView, ReviewRequest, Service, Side};
use akshually_testkit::TestRepo;
use akshually_ui::State;

mod common;

use common::{repo_with_one_edit, window};

/// A repository with uncommitted edits to `a.txt`, `b.txt` and the Generated `go.sum`, which
/// its Session's Changeset shows in that order.
fn repo_with_three_edits() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n").write("b.txt", "one\n").write("go.sum", "one\n").commit_all("base");
    repo.write("a.txt", "two\n").write("b.txt", "two\n").write("go.sum", "two\n");
    repo
}

/// Lines enough for the core to call a file Large (it allows 2,000 changed lines).
const LARGE: usize = 2_001;

/// Hands the service's Window to the window, as its subscriber does, and builds the next frame.
fn deliver_window_and_rebuild(rt: &mut Runtime<State>, service: &Service) {
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

/// Whether the `file`th file of the shown Changeset shows its diff: its first new line is there.
fn shows_diff(rt: &Runtime<State>, file: usize) -> bool {
    rt.find(&format!("df{file}.n1")).is_some()
}

#[test]
fn ticking_viewed_on_an_expanded_file_folds_its_diff() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let _pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    let mut rt = window(&service);
    assert!(shows_diff(&rt, 0), "the file didn't start expanded");

    press(&mut rt, "viewed-a.txt");

    assert!(!shows_diff(&rt, 0), "ticking Viewed left the file's diff showing");
}

#[test]
fn unticking_viewed_on_a_folded_file_unfolds_its_diff() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let _pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    let mut rt = window(&service);
    press(&mut rt, "viewed-a.txt");
    deliver_window_and_rebuild(&mut rt, &service);

    press(&mut rt, "viewed-a.txt");

    assert!(shows_diff(&rt, 0), "unticking Viewed left the file folded");
}

#[test]
fn a_session_opened_with_a_file_ticked_as_viewed_folds_only_that_file() {
    let repo = repo_with_three_edits();
    let service = Arc::new(Service::new());
    let pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    service.mark_viewed(pending.session(), "a.txt", true).unwrap();

    let rt = window(&service);

    assert!(!shows_diff(&rt, 0), "the viewed file opened expanded");
    assert!(shows_diff(&rt, 1), "the file not viewed opened folded");
}

#[test]
fn a_generated_file_not_viewed_still_opens_folded() {
    let repo = repo_with_three_edits();
    let service = Arc::new(Service::new());
    let _pending = service.review(ReviewRequest::new(repo.path())).unwrap();

    let rt = window(&service);

    assert!(!shows_diff(&rt, 2), "the Generated file opened expanded");
}

#[test]
fn a_large_file_ticked_as_viewed_opens_folded() {
    let repo = TestRepo::new();
    repo.write("big.txt", "").commit_all("base");
    repo.write("big.txt", &"line\n".repeat(LARGE));
    let service = Arc::new(Service::new());
    let pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    service.mark_viewed(pending.session(), "big.txt", true).unwrap();

    let rt = window(&service);

    assert!(!shows_diff(&rt, 0), "the viewed Large file opened expanded");
}

#[test]
fn after_a_new_round_an_unchanged_viewed_file_is_folded_and_a_changed_one_expanded() {
    let repo = repo_with_three_edits();
    let service = Arc::new(Service::new());
    let first = service.review(ReviewRequest::new(repo.path())).unwrap();
    let id = first.session().clone();
    service.mark_viewed(&id, "a.txt", true).unwrap();
    service.mark_viewed(&id, "b.txt", true).unwrap();
    let mut rt = window(&service);
    service.comment(&id, Anchor::lines(Side::New, "b.txt", 1..=1), "Why two?").unwrap();
    service.hand_over_round(&id).unwrap();
    assert!(first.wait_timeout(Duration::from_secs(5)).is_some(), "the Round wasn't handed over");

    repo.write("b.txt", "three\n");
    let _second = service.review(ReviewRequest::new(repo.path())).unwrap();
    service.show(&id, ChangesetView::Full).unwrap();
    deliver_window_and_rebuild(&mut rt, &service);

    assert!(!shows_diff(&rt, 0), "the unchanged viewed file came back expanded");
    assert!(shows_diff(&rt, 1), "the changed file, its tick cleared, came back folded");
}

#[test]
fn a_viewed_file_expanded_by_hand_stays_viewed_and_expanded_through_an_update() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    service.mark_viewed(pending.session(), "a.txt", true).unwrap();
    let mut rt = window(&service);
    // Rune's diff view keys each file header's fold button `toggle`; there is one file here.
    press(&mut rt, "toggle");
    assert!(shows_diff(&rt, 0), "the file header's toggle didn't expand the viewed file");

    service.comment(pending.session(), Anchor::lines(Side::New, "a.txt", 1..=1), "Why two?").unwrap();
    deliver_window_and_rebuild(&mut rt, &service);

    assert!(shows_diff(&rt, 0), "an update that left the Changeset shown alone folded the file again");
    assert!(service.session(pending.session()).unwrap().viewed.contains("a.txt"), "expanding the file unticked it");
}

#[test]
fn a_generated_file_ticked_as_viewed_opens_folded() {
    let repo = repo_with_three_edits();
    let service = Arc::new(Service::new());
    let pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    service.mark_viewed(pending.session(), "go.sum", true).unwrap();

    let rt = window(&service);

    assert!(!shows_diff(&rt, 2), "the viewed Generated file opened expanded");
}

#[test]
fn unticking_viewed_on_a_generated_file_unfolds_it() {
    let repo = repo_with_three_edits();
    let service = Arc::new(Service::new());
    let pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    service.mark_viewed(pending.session(), "go.sum", true).unwrap();
    let mut rt = window(&service);

    press(&mut rt, "viewed-go.sum");

    assert!(shows_diff(&rt, 2), "unticking Viewed left the Generated file folded");
}
