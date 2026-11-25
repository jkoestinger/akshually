//! UI: the scroll bar over a Session's diff in Rune's headless `Runtime`, against real Sessions
//! in the core service: whether it shows, where its thumb sits as the diff scrolls, how long it
//! is, and that dragging it scrolls the diff.

use std::sync::Arc;

use rune::accesskit::Action;
use rune::{Env, Runtime};

use akshually_core::{ReviewRequest, Service, SessionId};
use akshually_testkit::TestRepo;
use akshually_ui::{State, view};

/// The window's size, in points.
const WINDOW: (f32, f32) = (1280.0, 800.0);

/// The thumb's gap from the top and bottom of the diff, in points.
const INSET: f32 = 2.0;

/// The shortest thumb, in points.
const MIN_THUMB: f32 = 24.0;

/// A repository whose one file has `lines` lines, every one changed.
fn repo_with_changed_lines(lines: usize) -> TestRepo {
    let repo = TestRepo::new();
    let text = |suffix: &str| (0..lines).map(|i| format!("line {i}{suffix}\n")).collect::<String>();
    repo.write("a.txt", &text("")).commit_all("base");
    repo.write("a.txt", &text(" changed"));
    repo
}

/// Runs frames until the motion of the last input has settled: the thumb and the rows glide
/// to where layout puts them, and a frame of no time moves nothing.
fn settle(rt: &mut Runtime<State>) {
    for _ in 0..30 {
        rt.frame(0.05);
    }
}

/// A headless window over `service`, its first frame built.
fn window(service: &Arc<Service>) -> Runtime<State> {
    let mut rt = Runtime::new(State::new(service.clone()), view, WINDOW, Env::default());
    settle(&mut rt);
    rt
}

/// Opens a Session of `repo` and returns its id.
fn open(service: &Service, repo: &TestRepo) -> SessionId {
    service.review(ReviewRequest::new(repo.path())).unwrap().session().clone()
}

/// Hands the service's Window to the window, as its subscriber does, and rebuilds the view.
fn deliver_window(rt: &mut Runtime<State>, service: &Service) {
    let window = service.window();
    rt.update_if(move |s: &mut State, _| {
        s.apply(window);
        true
    });
    settle(rt);
}

/// Presses the element keyed `key` as an assistive client would.
fn press(rt: &mut Runtime<State>, key: &str) {
    assert!(rt.accessibility_action(key, Action::Click, None), "nothing to press at `{key}`");
    settle(rt);
}

/// Shows the tab of Session `id`.
fn open_tab(rt: &mut Runtime<State>, id: &SessionId) {
    press(rt, &format!("t-{id}"));
    assert!(rt.find(&format!("diff-{id}")).is_some(), "the tab of {id} didn't open");
}

/// Scrolls the shown diff to `y` points from its top, as jumping to a Thread or a file does.
fn scroll_to(rt: &mut Runtime<State>, id: &SessionId, y: f32) {
    rt.update(|_, tx| tx.scroll_to_y(format!("diff-{id}/scroll"), y));
    settle(rt);
}

/// The rect of the element keyed `key`, as drawn.
fn rect(rt: &Runtime<State>, key: &str) -> [f32; 4] {
    rt.presented(key).unwrap_or_else(|| panic!("nothing is drawn at `{key}`")).0
}

/// The diff's scroll viewport.
fn viewport(rt: &Runtime<State>) -> [f32; 4] {
    rect(rt, "scroll")
}

/// The thumb, if the diff shows a scroll bar.
fn thumb(rt: &Runtime<State>) -> Option<[f32; 4]> {
    rt.presented("diff-scroll-thumb").map(|(rect, _)| rect)
}

/// How far the shown diff has scrolled: how far its row list's top is above the viewport's.
fn scrolled(rt: &Runtime<State>) -> f32 {
    viewport(rt)[1] - rect(rt, "rows")[1]
}

#[test]
fn a_diff_taller_than_the_window_shows_a_scroll_bar() {
    let long = repo_with_changed_lines(400);
    let service = Arc::new(Service::new());
    let long_id = open(&service, &long);
    let mut rt = window(&service);
    deliver_window(&mut rt, &service);

    open_tab(&mut rt, &long_id);

    assert!(thumb(&rt).is_some(), "a diff of 400 lines showed no scroll bar");
}

#[test]
fn a_diff_that_fits_the_window_shows_no_scroll_bar() {
    let short = repo_with_changed_lines(2);
    let service = Arc::new(Service::new());
    let short_id = open(&service, &short);
    let mut rt = window(&service);
    deliver_window(&mut rt, &service);

    open_tab(&mut rt, &short_id);

    assert!(thumb(&rt).is_none(), "a diff that fits showed a scroll bar");
}

#[test]
fn the_thumb_sits_at_the_top_of_the_track_at_the_start_and_at_the_bottom_at_the_end() {
    let repo = repo_with_changed_lines(400);
    let service = Arc::new(Service::new());
    let id = open(&service, &repo);
    let mut rt = window(&service);
    deliver_window(&mut rt, &service);
    open_tab(&mut rt, &id);
    let [_, top, _, height] = viewport(&rt);

    let at_start = thumb(&rt).expect("a scroll bar at the start");
    scroll_to(&mut rt, &id, 1_000_000.0);
    let at_end = thumb(&rt).expect("a scroll bar at the end");

    assert_eq!(at_start[1], top + INSET, "the thumb isn't at the top of the track at the start");
    assert_eq!(at_end[1] + at_end[3], top + height - INSET, "the thumb isn't at the bottom of the track at the end");
}

#[test]
fn the_thumb_follows_the_wheel() {
    let repo = repo_with_changed_lines(400);
    let service = Arc::new(Service::new());
    let id = open(&service, &repo);
    let mut rt = window(&service);
    deliver_window(&mut rt, &service);
    open_tab(&mut rt, &id);
    let before = thumb(&rt).unwrap()[1];
    let (x, y) = rt.find("scroll").expect("the diff's viewport");

    rt.wheel(x, y, -400.0);
    settle(&mut rt);

    assert!(thumb(&rt).unwrap()[1] > before, "the thumb stayed put while the diff scrolled");
}

#[test]
fn a_longer_changeset_gives_a_shorter_thumb_never_below_the_minimum() {
    let service = Arc::new(Service::new());
    let (medium, long, huge) =
        (repo_with_changed_lines(100), repo_with_changed_lines(400), repo_with_changed_lines(900));
    let ids = [open(&service, &medium), open(&service, &long), open(&service, &huge)];
    let mut rt = window(&service);
    deliver_window(&mut rt, &service);

    let lengths = ids.map(|id| {
        open_tab(&mut rt, &id);
        thumb(&rt).expect("a scroll bar")[3]
    });

    assert!(lengths[0] > lengths[1], "the thumb of 100 lines isn't longer than that of 400: {lengths:?}");
    assert!(lengths[1] > lengths[2], "the thumb of 400 lines isn't longer than that of 900: {lengths:?}");
    assert_eq!(lengths[2], MIN_THUMB, "the thumb of 900 lines isn't the minimum length");
}

#[test]
fn dragging_the_thumb_scrolls_the_diff_to_the_matching_place() {
    let repo = repo_with_changed_lines(400);
    let service = Arc::new(Service::new());
    let id = open(&service, &repo);
    let mut rt = window(&service);
    deliver_window(&mut rt, &service);
    open_tab(&mut rt, &id);
    let [_, top, _, height] = viewport(&rt);
    let [x, y, w, len] = thumb(&rt).unwrap();
    let travel = height - 2.0 * INSET - len;
    let (grab_x, grab_y) = (x + w / 2.0, y + len / 2.0);

    rt.pointer_down(grab_x, grab_y);
    rt.pointer_move(grab_x, grab_y + travel / 2.0);
    settle(&mut rt);
    rt.pointer_up(grab_x, grab_y + travel / 2.0);
    settle(&mut rt);

    let [_, thumb_top, _, thumb_len] = thumb(&rt).unwrap();
    let halfway = top + INSET + travel / 2.0;
    assert!((thumb_top - halfway).abs() <= 1.0, "the thumb is at {thumb_top}, not halfway down the track at {halfway}");
    assert_eq!(thumb_len, len, "the thumb changed length while dragged");
    assert!(scrolled(&rt) > 0.0, "the diff didn't scroll with the thumb");
}

#[test]
fn the_diffs_rows_are_as_wide_with_the_bar_as_without_it() {
    let (long, short) = (repo_with_changed_lines(400), repo_with_changed_lines(2));
    let service = Arc::new(Service::new());
    let (long_id, short_id) = (open(&service, &long), open(&service, &short));
    let mut rt = window(&service);
    deliver_window(&mut rt, &service);

    open_tab(&mut rt, &short_id);
    let without = (rect(&rt, "rows")[2], viewport(&rt)[2]);
    open_tab(&mut rt, &long_id);
    let with = (rect(&rt, "rows")[2], viewport(&rt)[2]);

    assert_eq!(with, without, "the rows' and the viewport's widths changed with the bar");
    assert_eq!(with.0, with.1, "the rows are narrower than the viewport");
    let [thumb_x, _, thumb_w, _] = thumb(&rt).unwrap();
    assert!(thumb_x + thumb_w <= with.1 && thumb_x > with.1 - 20.0, "the thumb isn't over the diff's right edge");
}

#[test]
fn the_bar_follows_the_scroll_in_the_side_by_side_layout_too() {
    let repo = repo_with_changed_lines(400);
    let service = Arc::new(Service::new());
    let id = open(&service, &repo);
    let mut rt = window(&service);
    deliver_window(&mut rt, &service);
    open_tab(&mut rt, &id);
    press(&mut rt, "segment1");
    let [_, top, _, height] = viewport(&rt);

    let at_start = thumb(&rt).expect("a scroll bar at the start");
    scroll_to(&mut rt, &id, 1_000_000.0);
    let at_end = thumb(&rt).expect("a scroll bar at the end");

    assert_eq!(at_start[1], top + INSET);
    assert_eq!(at_end[1] + at_end[3], top + height - INSET);
}

/// Where the thumb should sit for how far the shown diff has scrolled.
fn thumb_top_for_the_scroll(rt: &Runtime<State>) -> f32 {
    let [_, top, _, height] = viewport(rt);
    let len = thumb(rt).expect("a scroll bar")[3];
    let (scrollable, travel) = (rect(rt, "rows")[3] - height, height - 2.0 * INSET - len);
    top + INSET + travel * scrolled(rt) / scrollable
}

#[test]
fn each_session_shows_the_thumb_for_the_position_its_diff_is_at_after_switching_tabs() {
    let (first, second) = (repo_with_changed_lines(400), repo_with_changed_lines(400));
    let service = Arc::new(Service::new());
    let (first_id, second_id) = (open(&service, &first), open(&service, &second));
    let mut rt = window(&service);
    deliver_window(&mut rt, &service);
    open_tab(&mut rt, &first_id);
    scroll_to(&mut rt, &first_id, 5_000.0);
    let first_thumb = thumb(&rt).unwrap()[1];

    open_tab(&mut rt, &second_id);
    let second_start = thumb(&rt).unwrap()[1];
    scroll_to(&mut rt, &second_id, 9_000.0);
    let second_scrolled = thumb(&rt).unwrap()[1];
    press(&mut rt, "t-inbox");
    open_tab(&mut rt, &first_id);

    assert!(first_thumb > second_start, "the first Session's scroll didn't move its thumb");
    assert!(second_scrolled > first_thumb, "the second Session's thumb isn't at its own, later position");
    assert!(
        (thumb(&rt).unwrap()[1] - thumb_top_for_the_scroll(&rt)).abs() <= 1.0,
        "the thumb isn't where the first Session's diff really is after the Inbox and back"
    );
}

#[test]
fn the_bar_shows_with_the_sticky_file_header_over_the_diff() {
    let repo = repo_with_changed_lines(400);
    let service = Arc::new(Service::new());
    let id = open(&service, &repo);
    let mut rt = window(&service);
    deliver_window(&mut rt, &service);
    open_tab(&mut rt, &id);

    scroll_to(&mut rt, &id, 5_000.0);

    assert!(rt.find("sticky").is_some(), "the file header isn't stuck at the top of the scrolled diff");
    assert!(thumb(&rt).is_some(), "the sticky file header took the scroll bar away");
}
