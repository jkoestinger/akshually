//! UI: the Rendered view of a changed markdown file, in Rune's headless `Runtime` against real
//! Sessions in the core service: which files offer it, what it shows in each Diff layout, its
//! line tints, and how it closes and lives on across tabs and reloads. The render smoke check
//! can't reach most of this: its scenes are fixed sample tabs over an empty service, so no
//! real Changeset is diffed, reloaded or scrolled there.

use std::sync::Arc;

use rune::accesskit::Action;
use rune::{Key, KeyEvent, Runtime};

use akshually_core::{Anchor, ReviewRequest, Service, SessionId, Side};
use akshually_testkit::TestRepo;
use akshually_ui::State;

mod common;

use common::window;

/// Presses the element keyed `key` as an assistive client would.
fn press(rt: &mut Runtime<State>, key: &str) {
    assert!(rt.accessibility_action(key, Action::Click, None), "nothing to press at `{key}`");
    rt.frame(0.0);
}

/// A repository whose `README.md` changed: its heading reworded, a line removed and one added.
fn repo_with_a_changed_readme() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("README.md", "# One\n\nKept paragraph.\n\nGone paragraph.\n").commit_all("base");
    repo.write("README.md", "# Two\n\nKept paragraph.\n\nNew paragraph.\n");
    repo
}

/// A Session of `repo`'s work, shown in a new window.
fn review(repo: &TestRepo) -> (Arc<Service>, SessionId, Runtime<State>) {
    let service = Arc::new(Service::new());
    let id = service.review(ReviewRequest::new(repo.path())).unwrap().session().clone();
    let rt = window(&service);
    (service, id, rt)
}

/// Hands the service's Window to the window, as its subscriber does, and rebuilds the view.
fn deliver_window(rt: &mut Runtime<State>, service: &Service) {
    let window = service.window();
    rt.update_if(move |s: &mut State, _| {
        s.apply(window);
        true
    });
    rt.frame(0.0);
}

/// Picks the side-by-side layout in the shown tab's header.
fn pick_side_by_side(rt: &mut Runtime<State>) {
    press(rt, "segment1");
}

/// Clicks at `(x, y)` in the window.
fn click(rt: &mut Runtime<State>, (x, y): (f32, f32)) {
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(0.0);
}

/// The Rendered view's tints named `kind` (`added` or `removed`), as the centres of their keyed
/// highlights, in order.
fn tints(rt: &Runtime<State>, kind: &str) -> Vec<(f32, f32)> {
    (0..).map_while(|i| rt.find(&format!("rendered-{kind}-{i}"))).collect()
}

/// The texts drawn inside the element keyed `key`, in tree order; empty when nothing has the key.
fn texts_within(rt: &Runtime<State>, key: &str) -> Vec<String> {
    let elements = rt.elements();
    let inside = |mut at: usize| loop {
        if elements[at].key.as_deref() == Some(key) {
            return true;
        }
        match elements[at].parent {
            Some(parent) => at = parent,
            None => return false,
        }
    };
    (0..elements.len()).filter(|&at| inside(at)).filter_map(|at| elements[at].text.clone()).collect()
}

#[test]
fn a_changed_markdown_file_offers_the_rendered_view_and_other_files_dont() {
    let repo = TestRepo::new();
    repo.write("README.md", "# One\n").write("lib.rs", "fn one() {}\n").commit_all("base");
    repo.write("README.md", "# Two\n").write("lib.rs", "fn two() {}\n");
    repo.write_bytes("logo.md", b"\x89PNG\0\0\x01");

    let (_service, _id, rt) = review(&repo);

    assert!(rt.find("rendered-README.md").is_some(), "the markdown file's header doesn't offer Rendered");
    assert!(rt.find("rendered-lib.rs").is_none(), "a Rust file's header offers Rendered");
    assert!(rt.find("rendered-logo.md").is_none(), "a binary file's header offers Rendered");
}

#[test]
fn a_collapsed_markdown_file_offers_the_rendered_view_too() {
    let repo = TestRepo::new();
    repo.write("API.md", "<!-- @generated -->\n# One\n").commit_all("base");
    repo.write("API.md", "<!-- @generated -->\n# Two\n");

    let (_service, _id, rt) = review(&repo);

    assert!(rt.find("rendered-API.md").is_some(), "the collapsed markdown file's header doesn't offer Rendered");
}

#[test]
fn pressing_rendered_shows_the_file_as_rendered_markdown() {
    let repo = repo_with_a_changed_readme();
    let (_service, _id, mut rt) = review(&repo);

    press(&mut rt, "rendered-README.md");

    let texts = texts_within(&rt, "rendered-view");
    assert!(texts.iter().any(|t| t == "Two"), "the heading isn't rendered: {texts:?}");
    assert!(!texts.iter().any(|t| t.contains("# Two")), "the file shows as raw lines: {texts:?}");
}

#[test]
fn the_rendered_view_is_nearly_as_wide_as_the_window() {
    let repo = repo_with_a_changed_readme();
    let (_service, _id, mut rt) = review(&repo);

    press(&mut rt, "rendered-README.md");

    let view = rt.elements().into_iter().find(|e| e.key.as_deref() == Some("rendered-view")).unwrap();
    assert!(view.rect[2] >= 1200.0, "the Rendered view is {} points wide in a 1280-point window", view.rect[2]);
}

#[test]
fn side_by_side_shows_the_old_version_beside_the_new() {
    let repo = repo_with_a_changed_readme();
    let (_service, _id, mut rt) = review(&repo);
    pick_side_by_side(&mut rt);

    press(&mut rt, "rendered-README.md");

    let (old, new) = (texts_within(&rt, "rendered-old"), texts_within(&rt, "rendered-new"));
    assert!(old.iter().any(|t| t == "One") && old.iter().any(|t| t == "Gone paragraph."), "old column: {old:?}");
    assert!(new.iter().any(|t| t == "Two") && new.iter().any(|t| t == "New paragraph."), "new column: {new:?}");
}

#[test]
fn side_by_side_tints_the_removed_lines_on_the_old_side_and_the_added_lines_on_the_new() {
    let repo = repo_with_a_changed_readme();
    let (_service, _id, mut rt) = review(&repo);
    pick_side_by_side(&mut rt);

    press(&mut rt, "rendered-README.md");

    let (removed, added) = (tints(&rt, "removed"), tints(&rt, "added"));
    assert_eq!((removed.len(), added.len()), (2, 2), "the heading and last paragraph are tinted on each side");
    let (old_x, _) = rt.find("rendered-old").unwrap();
    let (new_x, _) = rt.find("rendered-new").unwrap();
    let side = |x: f32| if (x - old_x).abs() < (x - new_x).abs() { "old" } else { "new" };
    assert!(removed.iter().all(|&(x, _)| side(x) == "old"), "a removed tint isn't on the old side");
    assert!(added.iter().all(|&(x, _)| side(x) == "new"), "an added tint isn't on the new side");
}

#[test]
fn stacked_shows_the_new_version_alone_and_how_many_lines_were_removed() {
    let repo = repo_with_a_changed_readme();
    let (_service, _id, mut rt) = review(&repo);

    press(&mut rt, "rendered-README.md");

    assert!(rt.find("rendered-old").is_none(), "stacked shows the old version");
    assert!(rt.find("rendered-new").is_some(), "stacked doesn't show the new version");
    assert_eq!(tints(&rt, "added").len(), 2, "the added lines aren't tinted");
    assert_eq!(texts_within(&rt, "rendered-removed-note"), ["2 lines removed, see the diff"]);
}

#[test]
fn stacked_says_nothing_about_removed_lines_when_none_were() {
    let repo = TestRepo::new();
    repo.write("README.md", "# One\n").commit_all("base");
    repo.write("README.md", "# One\n\nMore.\n");
    let (_service, _id, mut rt) = review(&repo);

    press(&mut rt, "rendered-README.md");

    assert!(rt.find("rendered-removed-note").is_none(), "a note about removed lines when none were");
}

#[test]
fn a_wholly_added_file_shows_its_new_version_alone_untinted_even_side_by_side() {
    let repo = TestRepo::new();
    repo.write("other.txt", "x\n").commit_all("base");
    repo.write("NOTES.markdown", "# Notes\n\nAll new.\n");
    let (_service, _id, mut rt) = review(&repo);
    pick_side_by_side(&mut rt);

    press(&mut rt, "rendered-NOTES.markdown");

    assert!(rt.find("rendered-old").is_none(), "an added file shows an old version");
    assert!(texts_within(&rt, "rendered-new").iter().any(|t| t == "All new."), "the new version doesn't show");
    assert!(tints(&rt, "added").is_empty(), "an added file is tinted");
}

#[test]
fn a_wholly_deleted_file_shows_its_old_version_alone_untinted() {
    let repo = TestRepo::new();
    repo.write("Guide.MD", "# Guide\n\nAll gone.\n").commit_all("base");
    repo.remove("Guide.MD");
    let (_service, _id, mut rt) = review(&repo);
    pick_side_by_side(&mut rt);

    press(&mut rt, "rendered-Guide.MD");

    assert!(rt.find("rendered-new").is_none(), "a deleted file shows a new version");
    assert!(texts_within(&rt, "rendered-old").iter().any(|t| t == "All gone."), "the old version doesn't show");
    assert!(tints(&rt, "removed").is_empty(), "a deleted file is tinted");
}

#[test]
fn the_header_counts_the_open_threads_on_the_file() {
    let repo = repo_with_a_changed_readme();
    repo.write("lib.rs", "fn one() {}\n");
    let (service, id, mut rt) = review(&repo);
    service.comment(&id, Anchor::lines(Side::New, "README.md", 5..=5), "On a line.").unwrap();
    service.comment(&id, Anchor::file("README.md"), "On the whole file.").unwrap();
    service.comment(&id, Anchor::file("lib.rs"), "Another file.").unwrap();
    service.comment(&id, Anchor::Review, "The Review.").unwrap();
    deliver_window(&mut rt, &service);

    press(&mut rt, "rendered-README.md");

    assert_eq!(texts_within(&rt, "rendered-threads"), ["2 open Threads"]);
}

#[test]
fn the_header_hides_the_thread_count_without_open_threads() {
    let repo = repo_with_a_changed_readme();
    let (_service, _id, mut rt) = review(&repo);

    press(&mut rt, "rendered-README.md");

    assert!(rt.find("rendered-threads").is_none(), "the header counts no open Threads");
}

/// A repository whose long `README.md` changed on every line, so its diff scrolls.
fn repo_with_a_long_readme() -> TestRepo {
    let lines = |word: &str| -> String { (1..=100).map(|n| format!("Line {n}, {word}.\n")).collect() };
    let repo = TestRepo::new();
    repo.write("README.md", &lines("before")).commit_all("base");
    repo.write("README.md", &lines("after"));
    repo
}

/// How far down the diff is scrolled: the furthest any element in the window is carried up by
/// scrolling, as the Rendered view's own scroll view is never scrolled here.
fn diff_scroll(rt: &Runtime<State>) -> f32 {
    rt.elements().into_iter().map(|e| e.scrolled[1]).fold(0.0, f32::max)
}

/// Opens the Rendered view over a scrolled diff, returning how far the diff was scrolled.
fn open_over_a_scrolled_diff(rt: &mut Runtime<State>, id: &SessionId) -> f32 {
    let (x, y) = rt.find(&format!("diff-{id}")).unwrap();
    rt.wheel(x, y, -400.0);
    rt.frame(0.0);
    let scrolled = diff_scroll(rt);
    assert!(scrolled > 0.0, "the diff didn't scroll");
    press(rt, "rendered-README.md");
    assert!(rt.find("rendered-view").is_some(), "the Rendered view didn't open");
    scrolled
}

#[test]
fn escape_closes_the_rendered_view_leaving_the_diff_as_it_was() {
    let repo = repo_with_a_long_readme();
    let (_service, id, mut rt) = review(&repo);
    let scrolled = open_over_a_scrolled_diff(&mut rt, &id);

    rt.key(KeyEvent { key: Key::Escape, mods: Default::default() });
    rt.frame(0.0);

    assert!(rt.find("rendered-view").is_none(), "Escape left the Rendered view open");
    assert_eq!(diff_scroll(&rt), scrolled, "the diff moved");
}

#[test]
fn the_close_button_closes_the_rendered_view_leaving_the_diff_as_it_was() {
    let repo = repo_with_a_long_readme();
    let (_service, id, mut rt) = review(&repo);
    let scrolled = open_over_a_scrolled_diff(&mut rt, &id);

    press(&mut rt, "rendered-close");

    assert!(rt.find("rendered-view").is_none(), "✕ left the Rendered view open");
    assert_eq!(diff_scroll(&rt), scrolled, "the diff moved");
}

#[test]
fn a_click_outside_closes_the_rendered_view_leaving_the_diff_as_it_was() {
    let repo = repo_with_a_long_readme();
    let (_service, id, mut rt) = review(&repo);
    let scrolled = open_over_a_scrolled_diff(&mut rt, &id);

    click(&mut rt, (4.0, 400.0));

    assert!(rt.find("rendered-view").is_none(), "a click outside left the Rendered view open");
    assert_eq!(diff_scroll(&rt), scrolled, "the diff moved");
}

/// Two Sessions with a changed `README.md`, the first shown with its Rendered view open; the
/// second's id.
fn rendered_view_open_beside_another_session(
    repos: &(TestRepo, TestRepo),
) -> (Arc<Service>, SessionId, SessionId, Runtime<State>) {
    let (service, id, mut rt) = review(&repos.0);
    let other = service.review(ReviewRequest::new(repos.1.path())).unwrap().session().clone();
    deliver_window(&mut rt, &service);
    press(&mut rt, "rendered-README.md");
    (service, id, other, rt)
}

#[test]
fn the_rendered_view_doesnt_show_over_another_sessions_tab() {
    let repos = (repo_with_a_changed_readme(), repo_with_a_changed_readme());
    let (service, _id, other, mut rt) = rendered_view_open_beside_another_session(&repos);

    service.display(&other).unwrap();
    deliver_window(&mut rt, &service);

    assert!(rt.find("rendered-view").is_none(), "the Rendered view shows over another Session");
}

#[test]
fn the_rendered_view_stays_open_through_switching_tabs_and_back() {
    let repos = (repo_with_a_changed_readme(), repo_with_a_changed_readme());
    let (service, id, other, mut rt) = rendered_view_open_beside_another_session(&repos);

    service.display(&other).unwrap();
    deliver_window(&mut rt, &service);
    service.display(&id).unwrap();
    deliver_window(&mut rt, &service);

    assert!(rt.find("rendered-view").is_some(), "switching tabs closed the Rendered view");
}

#[test]
fn reloading_the_changeset_renders_the_new_text() {
    let repo = repo_with_a_changed_readme();
    let (service, id, mut rt) = review(&repo);
    press(&mut rt, "rendered-README.md");

    repo.write("README.md", "# Three\n");
    service.reload(&id).unwrap();
    deliver_window(&mut rt, &service);

    assert!(texts_within(&rt, "rendered-new").iter().any(|t| t == "Three"), "the reloaded text doesn't show");
}

#[test]
fn reloading_a_changeset_without_the_file_closes_the_rendered_view() {
    let repo = repo_with_a_changed_readme();
    repo.write("lib.rs", "fn one() {}\n");
    let (service, id, mut rt) = review(&repo);
    press(&mut rt, "rendered-README.md");

    repo.write("README.md", "# One\n\nKept paragraph.\n\nGone paragraph.\n");
    service.reload(&id).unwrap();
    deliver_window(&mut rt, &service);
    repo.write("README.md", "# Back\n");
    service.reload(&id).unwrap();
    deliver_window(&mut rt, &service);

    assert!(rt.find("rendered-view").is_none(), "the Rendered view came back with its file");
}
