//! UI: Search in the code of the Changeset a Session shows, in Rune's headless `Runtime` against
//! real Sessions in the core service: opening the search field, counting and stepping through
//! matches, what is and isn't searched, folded files, the Rendered view, and how the query lives
//! on across tabs and reloads. The render smoke check can't reach this: its scenes are fixed
//! sample tabs, with no real Changeset to search, scroll or reload.

use std::sync::Arc;

use rune::accesskit::Action;
use rune::{Env, Key, KeyEvent, Mods, Runtime};

use akshually_core::{Anchor, ReviewRequest, Service, SessionId, Side};
use akshually_testkit::TestRepo;
use akshually_ui::{State, view};

/// A headless window over `service`, its first frame built, with the app's key handler wired.
fn window(service: &Arc<Service>) -> Runtime<State> {
    let mut rt = Runtime::new(State::new(service.clone()), view, (1280.0, 800.0), Env::default());
    rt.on_key = Some(akshually_ui::on_key);
    rt.frame(0.0);
    rt
}

/// A Session of `repo`'s work, shown in a new window.
fn review(repo: &TestRepo) -> (Arc<Service>, SessionId, Runtime<State>) {
    let service = Arc::new(Service::new());
    let id = service.review(ReviewRequest::new(repo.path())).unwrap().session().clone();
    let rt = window(&service);
    (service, id, rt)
}

/// Presses the element keyed `key` as an assistive client would.
fn press(rt: &mut Runtime<State>, key: &str) {
    assert!(rt.accessibility_action(key, Action::Click, None), "nothing to press at `{key}`");
    rt.frame(0.0);
}

/// Sends `key` with `mods`, then builds the next frame.
fn send(rt: &mut Runtime<State>, key: Key, mods: Mods) {
    rt.key(KeyEvent { key, mods });
    rt.frame(0.0);
}

/// Presses Ctrl+F (Cmd+F on macOS: Rune's `cmd` is the primary modifier on both).
fn find_chord(rt: &mut Runtime<State>) {
    send(rt, Key::Command('f'), Mods { cmd: true, ctrl: !cfg!(target_os = "macos"), ..Mods::default() });
}

/// Types `text` one character at a time.
fn type_text(rt: &mut Runtime<State>, text: &str) {
    for c in text.chars() {
        send(rt, Key::Text(c.to_string()), Mods::default());
    }
}

fn press_enter(rt: &mut Runtime<State>) {
    send(rt, Key::Enter, Mods::default());
}

fn press_shift_enter(rt: &mut Runtime<State>) {
    send(rt, Key::Enter, Mods { shift: true, ..Mods::default() });
}

fn press_escape(rt: &mut Runtime<State>) {
    send(rt, Key::Escape, Mods::default());
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

/// What the search field says about its matches: `3 of 12`, `No matches`, or nothing.
fn count(rt: &Runtime<State>) -> String {
    texts_within(rt, "search-count").join("")
}

/// The window's height, in points.
const WINDOW_HEIGHT: f32 = 800.0;

/// Whether the diff shows the line with key `row` (`df0.n95`: file 0, new line 95) within the
/// window: the diff mounts rows a screen or two beyond it too.
fn shows(rt: &Runtime<State>, row: &str) -> bool {
    rt.find(row).is_some_and(|(_, y)| (0.0..WINDOW_HEIGHT).contains(&y))
}

/// A repository whose `a.txt` changed on every one of its 100 lines, two of which now say
/// "needle": the 5th and the 95th. The diff is far taller than the window, so the 95th is out
/// of view until something scrolls to it.
fn repo_with_two_needles() -> TestRepo {
    let repo = TestRepo::new();
    let old: String = (1..=100).map(|i| format!("old {i}\n")).collect();
    repo.write("a.txt", &old).commit_all("base");
    let new: String =
        (1..=100).map(|i| if i == 5 || i == 95 { format!("new needle {i}\n") } else { format!("new {i}\n") }).collect();
    repo.write("a.txt", &new);
    repo
}

#[test]
fn ctrl_f_opens_the_search_field_in_a_displayed_session() {
    let repo = repo_with_two_needles();
    let (_service, _id, mut rt) = review(&repo);
    assert!(rt.find("search-field").is_none(), "the search field was open from the start");

    find_chord(&mut rt);

    assert!(rt.find("search-field").is_some(), "Ctrl+F didn't open the search field");
}

#[test]
fn ctrl_f_does_nothing_on_the_inbox() {
    let service = Arc::new(Service::new());
    let mut rt = window(&service);

    find_chord(&mut rt);

    assert!(rt.find("search-field").is_none(), "the Inbox opened a search field");
}

#[test]
fn typing_a_query_counts_its_matches_and_shows_the_first_as_current() {
    let repo = repo_with_two_needles();
    let (_service, _id, mut rt) = review(&repo);
    find_chord(&mut rt);

    type_text(&mut rt, "needle");

    assert_eq!(count(&rt), "1 of 2");
}

#[test]
fn a_query_with_no_match_says_so() {
    let repo = repo_with_two_needles();
    let (_service, _id, mut rt) = review(&repo);
    find_chord(&mut rt);

    type_text(&mut rt, "haystack");

    assert_eq!(count(&rt), "No matches");
}

#[test]
fn an_empty_query_has_no_matches_to_count() {
    let repo = repo_with_two_needles();
    let (_service, _id, mut rt) = review(&repo);

    find_chord(&mut rt);

    assert_eq!(count(&rt), "", "an empty query counted matches");
}

#[test]
fn enter_goes_to_the_next_match_and_wraps_round_at_the_end() {
    let repo = repo_with_two_needles();
    let (_service, _id, mut rt) = review(&repo);
    find_chord(&mut rt);
    type_text(&mut rt, "needle");

    press_enter(&mut rt);
    assert_eq!(count(&rt), "2 of 2");
    press_enter(&mut rt);

    assert_eq!(count(&rt), "1 of 2", "Enter at the last match didn't wrap to the first");
}

#[test]
fn shift_enter_goes_to_the_previous_match_and_wraps_round_at_the_start() {
    let repo = repo_with_two_needles();
    let (_service, _id, mut rt) = review(&repo);
    find_chord(&mut rt);
    type_text(&mut rt, "needle");

    press_shift_enter(&mut rt);

    assert_eq!(count(&rt), "2 of 2", "Shift+Enter at the first match didn't wrap to the last");
}

#[test]
fn going_to_a_match_scrolls_its_line_into_view() {
    let repo = repo_with_two_needles();
    let (_service, _id, mut rt) = review(&repo);
    find_chord(&mut rt);
    type_text(&mut rt, "needle");
    assert!(!shows(&rt, "df0.n95"), "the 95th line was in view before going to it");

    press_enter(&mut rt);

    assert!(shows(&rt, "df0.n95"), "going to the match didn't scroll its line into view");
}

#[test]
fn escape_closes_the_search_field() {
    let repo = repo_with_two_needles();
    let (_service, _id, mut rt) = review(&repo);
    find_chord(&mut rt);

    press_escape(&mut rt);

    assert!(rt.find("search-field").is_none(), "Escape left the search field open");
}

/// A repository whose `a.txt` has two hunks: its first 60 lines changed, and the 80th, with 13
/// unchanged lines between them, the 70th of which says "treasure". The second hunk is below the
/// window until something scrolls to it.
fn repo_with_two_hunks() -> TestRepo {
    let repo = TestRepo::new();
    let line = |i: u32, word: &str| if i == 70 { "treasure".to_owned() } else { format!("{word} {i}") };
    let old: String = (1..=100).map(|i| format!("{}\n", line(i, "line"))).collect();
    repo.write("a.txt", &old).commit_all("base");
    let new: String = (1..=100)
        .map(|i| format!("{}\n", if i <= 60 || i == 80 { line(i, "changed") } else { line(i, "line") }))
        .collect();
    repo.write("a.txt", &new);
    repo
}

#[test]
fn typing_in_the_search_field_never_triggers_the_diffs_own_keys() {
    let repo = repo_with_two_hunks();
    let (_service, _id, mut rt) = review(&repo);
    find_chord(&mut rt);
    assert!(!shows(&rt, "df0.n80"), "the second hunk was in view before any key");

    // Two `j`s would take the diff to the second hunk, as `after_escape_the_diffs_own_keys_work_again` has it.
    type_text(&mut rt, "jj][");

    assert_eq!(count(&rt), "No matches", "the keys didn't land in the search field");
    assert!(!shows(&rt, "df0.n80"), "a diff key moved the diff while the search field had focus");
}

#[test]
fn after_escape_the_diffs_own_keys_work_again() {
    let repo = repo_with_two_hunks();
    let (_service, _id, mut rt) = review(&repo);
    find_chord(&mut rt);
    press_escape(&mut rt);
    assert!(!shows(&rt, "df0.n80"), "the second hunk was in view before `j`");

    // The first `j` goes to the first hunk, already in view; the second to the next.
    type_text(&mut rt, "jj");

    assert!(shows(&rt, "df0.n80"), "`j` didn't go to the next hunk once the search field was closed");
}

#[test]
fn matches_are_found_on_both_sides_ignoring_case_and_accents() {
    let repo = TestRepo::new();
    repo.write("a.txt", "Résumé draft\nkept\n").commit_all("base");
    repo.write("a.txt", "RESUME final\nkept\n");
    let (_service, _id, mut rt) = review(&repo);
    find_chord(&mut rt);

    type_text(&mut rt, "resume");

    assert_eq!(count(&rt), "1 of 2", "the old and new sides should each match");
}

#[test]
fn a_query_with_an_accent_matches_only_that_accent() {
    let repo = TestRepo::new();
    repo.write("a.txt", "Résumé draft\nkept\n").commit_all("base");
    repo.write("a.txt", "RESUME final\nkept\n");
    let (_service, _id, mut rt) = review(&repo);
    find_chord(&mut rt);

    type_text(&mut rt, "résumé");

    assert_eq!(count(&rt), "1 of 1");
}

#[test]
fn text_in_an_unexpanded_unchanged_stretch_is_not_matched_until_it_is_expanded() {
    let repo = repo_with_two_hunks();
    let (_service, _id, mut rt) = review(&repo);
    find_chord(&mut rt);
    type_text(&mut rt, "treasure");
    assert_eq!(count(&rt), "No matches", "hidden text was matched");
    for _ in 0..4 {
        rt.wheel(640.0, 500.0, -600.0);
        for _ in 0..60 {
            rt.frame(1.0 / 60.0);
        }
    }

    let all = rt.elements().into_iter().find(|e| e.path.ends_with("df0.g1/inner/expand/All"));
    let all = all.expect("the hidden stretch offers no button to show all of it");
    let (x, y) = (all.rect[0] + all.rect[2] / 2.0, all.rect[1] + all.rect[3] / 2.0);
    rt.pointer_move(x, y);
    rt.frame(0.0);
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(0.0);
    for _ in 0..10 {
        rt.frame(1.0 / 60.0);
    }

    assert_eq!(count(&rt), "1 of 1", "the expanded stretch wasn't searched");
}

#[test]
fn text_only_in_a_comment_is_not_matched() {
    let repo = repo_with_two_needles();
    let service = Arc::new(Service::new());
    let id = service.review(ReviewRequest::new(repo.path())).unwrap().session().clone();
    service.comment(&id, Anchor::lines(Side::New, "a.txt", 1..=1), "zebra stripes").unwrap();
    let mut rt = window(&service);
    find_chord(&mut rt);

    type_text(&mut rt, "zebra");

    assert_eq!(count(&rt), "No matches", "a Comment's text was matched");
}

/// A repository whose `a.txt` is as `repo_with_two_needles`'s, with a second file after it.
fn repo_with_a_second_file() -> TestRepo {
    let repo = repo_with_two_needles();
    repo.write("b.txt", "one\n");
    repo
}

#[test]
fn a_match_in_a_folded_file_counts_and_going_to_it_unfolds_the_file_but_keeps_its_viewed_mark() {
    let repo = repo_with_a_second_file();
    let (service, _id, mut rt) = review(&repo);
    press(&mut rt, "viewed-a.txt");
    assert!(rt.find("df0.n5").is_none(), "ticking Viewed didn't fold the file");
    find_chord(&mut rt);

    type_text(&mut rt, "needle");

    assert_eq!(count(&rt), "1 of 2", "the folded file's matches weren't counted");
    assert!(shows(&rt, "df0.n5"), "going to the match didn't unfold the file");
    let viewed = &service.window().sessions[0].viewed;
    assert!(viewed.iter().any(|path| path == "a.txt"), "unfolding the file cleared its viewed mark");
}

#[test]
fn each_tab_has_its_own_query_and_keeps_it_when_switched_away_and_back() {
    let (repo_a, repo_b) = (repo_with_two_needles(), repo_with_two_needles());
    let service = Arc::new(Service::new());
    let a = service.review(ReviewRequest::new(repo_a.path())).unwrap().session().clone();
    let b = service.review(ReviewRequest::new(repo_b.path())).unwrap().session().clone();
    let mut rt = window(&service);
    press(&mut rt, &format!("t-{a}"));
    find_chord(&mut rt);
    type_text(&mut rt, "needle");

    press(&mut rt, &format!("t-{b}"));
    assert!(rt.find("search-field").is_none(), "another tab showed the search field");
    press(&mut rt, &format!("t-{a}"));

    assert_eq!(count(&rt), "1 of 2", "the query was lost when switching tabs");
}

/// A repository whose `README.md` and `lib.rs` both changed, and both say "paragraph", the
/// README in markup: a heading, a link and bold text.
fn repo_with_a_readme_and_code() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("README.md", "# One\n\nGone paragraph.\n").write("lib.rs", "// paragraph one\n").commit_all("base");
    repo.write("README.md", "# Two\n\nA **bold** paragraph, and [another paragraph](https://example.com/paragraph).\n");
    repo.write("lib.rs", "// paragraph two\n");
    repo
}

#[test]
fn with_a_rendered_view_open_only_its_files_rendered_text_is_searched() {
    let repo = repo_with_a_readme_and_code();
    let (_service, _id, mut rt) = review(&repo);
    find_chord(&mut rt);
    type_text(&mut rt, "paragraph");
    assert_eq!(count(&rt), "1 of 6", "the diff has three in the README's new line, one in its old, two in lib.rs");

    press(&mut rt, "rendered-README.md");

    assert_eq!(count(&rt), "1 of 2", "the link's address or another file was searched");
}

#[test]
fn markup_that_isnt_rendered_is_not_searched() {
    let repo = repo_with_a_readme_and_code();
    let (_service, _id, mut rt) = review(&repo);
    press(&mut rt, "rendered-README.md");
    find_chord(&mut rt);

    type_text(&mut rt, "**");

    assert_eq!(count(&rt), "No matches", "the heading or emphasis markers were searched");
}

#[test]
fn the_rendered_view_highlights_its_matches_and_the_current_one_distinctly() {
    let repo = repo_with_a_readme_and_code();
    let (_service, _id, mut rt) = review(&repo);
    press(&mut rt, "rendered-README.md");
    find_chord(&mut rt);

    type_text(&mut rt, "paragraph");

    assert!(rt.find("search-current").is_some(), "no match is highlighted as the current one");
    assert!(rt.find("search-match-1").is_some(), "the other match isn't highlighted");
    let first = rt.find("search-current").unwrap();
    press_enter(&mut rt);
    let second = rt.find("search-current").unwrap();
    assert_ne!(first, second, "the current highlight didn't move to the next match");
}

#[test]
fn the_rendered_view_scrolls_to_the_current_match() {
    let repo = TestRepo::new();
    repo.write("README.md", "# One\n").commit_all("base");
    let long: String =
        (1..=120).map(|i| if i == 110 { "A needle.\n\n".to_owned() } else { format!("Paragraph {i}.\n\n") }).collect();
    repo.write("README.md", &long);
    let (_service, _id, mut rt) = review(&repo);
    press(&mut rt, "rendered-README.md");
    find_chord(&mut rt);

    type_text(&mut rt, "needle");
    for _ in 0..60 {
        rt.frame(1.0 / 60.0);
    }

    let (_, y) = rt.find("search-current").expect("the match isn't highlighted");
    assert!((0.0..WINDOW_HEIGHT).contains(&y), "the Rendered view didn't scroll to the match, which is at y={y}");
}

#[test]
fn closing_the_rendered_view_searches_the_diff_again() {
    let repo = repo_with_a_readme_and_code();
    let (_service, _id, mut rt) = review(&repo);
    press(&mut rt, "rendered-README.md");
    find_chord(&mut rt);
    type_text(&mut rt, "paragraph");
    assert_eq!(count(&rt), "1 of 2");

    press(&mut rt, "rendered-close");

    assert_eq!(count(&rt), "1 of 6", "the search stayed on the Rendered view");
}

#[test]
fn escape_in_the_rendered_views_search_field_closes_the_search_and_keeps_the_view() {
    let repo = repo_with_a_readme_and_code();
    let (_service, _id, mut rt) = review(&repo);
    press(&mut rt, "rendered-README.md");
    find_chord(&mut rt);

    press_escape(&mut rt);

    assert!(rt.find("search-field").is_none(), "Escape left the search field open");
    assert!(rt.find("rendered-view").is_some(), "Escape closed the Rendered view as well");
}

#[test]
fn after_the_changeset_reloads_the_matches_reflect_the_new_changeset() {
    let repo = repo_with_two_needles();
    let (service, id, mut rt) = review(&repo);
    find_chord(&mut rt);
    type_text(&mut rt, "needle");
    assert_eq!(count(&rt), "1 of 2");
    let new: String =
        (1..=100).map(|i| if i == 5 { "new needle 5\n".to_owned() } else { format!("new {i}\n") }).collect();
    repo.write("a.txt", &new);

    service.reload(&id).unwrap();
    let window = service.window();
    rt.update_if(move |s: &mut State, _| {
        s.apply(window);
        true
    });
    rt.frame(0.0);

    assert_eq!(count(&rt), "1 of 1", "the count didn't follow the reloaded Changeset");
}
