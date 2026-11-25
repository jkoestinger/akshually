//! UI: the `n` and `p` keys and the two header buttons that go to the next and previous
//! unresolved Thread of the diff (#103), in Rune's headless `Runtime` against real Sessions in
//! the core service: which Threads are visited, in what order, from where, and when the keys
//! are text or do nothing.

use std::sync::Arc;
use std::time::Duration;

use rune::accesskit::Action;
use rune::{Env, Key, KeyEvent, Mods, Runtime};

use akshually_core::forge::{ForgeAdapter, ForgeError, PullRequest, PullRequestRef, RemoteThread, Review};
use akshually_core::{
    Anchor, Changeset, ChangesetView, FileChange, ReviewRequest, Service, SessionId, Side, ThreadId, Verdict,
};
use akshually_testkit::TestRepo;
use akshually_ui::{State, view};

/// A headless window over `service`, its first frame built, with the app's key handler wired.
fn window(service: &Arc<Service>) -> Runtime<State> {
    let mut rt = Runtime::new(State::new(service.clone()), view, (1280.0, 800.0), Env::default());
    rt.on_key = Some(akshually_ui::on_key);
    rt.frame(0.0);
    rt
}

/// Hands the service's Window to the window, as its subscriber does, and builds the next frame.
fn deliver_window(rt: &mut Runtime<State>, service: &Service) {
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

/// Types `text` one character at a time, then builds the next frame.
fn type_text(rt: &mut Runtime<State>, text: &str) {
    for c in text.chars() {
        rt.key(KeyEvent { key: Key::Text(c.to_string()), mods: Mods::default() });
        rt.frame(0.0);
    }
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

/// The window's height, in points.
const WINDOW_HEIGHT: f32 = 800.0;

/// Whether the diff shows the line with key `row` (`df0.n95`: file 0, new line 95) within the
/// window: the diff mounts rows a screen or two beyond it too.
fn shows(rt: &Runtime<State>, row: &str) -> bool {
    rt.find(row).is_some_and(|(_, y)| (0.0..WINDOW_HEIGHT).contains(&y))
}

/// The Threads' lines, each far enough from the next to be out of view from it.
const THREAD_LINES: [u32; 3] = [5, 50, 95];

/// A repository whose `a.txt` changed on every one of its 100 lines.
fn repo_with_a_long_change() -> TestRepo {
    let repo = TestRepo::new();
    let old: String = (1..=100).map(|i| format!("old {i}\n")).collect();
    repo.write("a.txt", &old).commit_all("base");
    let new: String = (1..=100).map(|i| format!("new {i}\n")).collect();
    repo.write("a.txt", &new);
    repo
}

/// A Session of `repo`'s work, shown in a new window, with a Draft Thread `T1`, `T2`, `T3` on
/// each of `THREAD_LINES` of `a.txt`.
fn review_with_three_threads(repo: &TestRepo) -> (Arc<Service>, SessionId, Runtime<State>) {
    let service = Arc::new(Service::new());
    let id = service.review(ReviewRequest::new(repo.path())).unwrap().session().clone();
    for line in THREAD_LINES {
        service.comment(&id, Anchor::lines(Side::New, "a.txt", line..=line), "Why?").unwrap();
    }
    let rt = window(&service);
    (service, id, rt)
}

/// Which of `THREAD_LINES` the diff shows in the window, as the lines.
fn lines_in_view(rt: &Runtime<State>) -> Vec<u32> {
    THREAD_LINES.into_iter().filter(|line| shows(rt, &format!("df0.n{line}"))).collect()
}

#[test]
fn n_scrolls_to_the_next_thread_below_the_view_and_p_to_the_previous_one_above_it() {
    let repo = repo_with_a_long_change();
    let (_service, _id, mut rt) = review_with_three_threads(&repo);
    type_text(&mut rt, "n");
    assert_eq!(lines_in_view(&rt), [5], "the first `n` didn't go to the first Thread");
    type_text(&mut rt, "n");
    assert_eq!(lines_in_view(&rt), [50]);

    type_text(&mut rt, "p");

    assert_eq!(lines_in_view(&rt), [5], "`p` didn't go back to the Thread above");
}

#[test]
fn pressing_n_repeatedly_visits_every_thread_in_the_diffs_order_then_wraps() {
    let repo = repo_with_a_long_change();
    let (_service, _id, mut rt) = review_with_three_threads(&repo);

    let mut visited = Vec::new();
    for _ in 0..4 {
        type_text(&mut rt, "n");
        visited.push(lines_in_view(&rt));
    }

    assert_eq!(visited, [[5], [50], [95], [5]], "`n` skipped a Thread or didn't wrap");
}

#[test]
fn p_from_the_start_wraps_to_the_last_thread_and_walks_back() {
    let repo = repo_with_a_long_change();
    let (_service, _id, mut rt) = review_with_three_threads(&repo);

    let mut visited = Vec::new();
    for _ in 0..3 {
        type_text(&mut rt, "p");
        visited.push(lines_in_view(&rt));
    }

    assert_eq!(visited, [[95], [50], [5]], "`p` skipped a Thread or didn't wrap");
}

#[test]
fn with_one_thread_both_keys_go_to_it() {
    let repo = repo_with_a_long_change();
    let service = Arc::new(Service::new());
    let id = service.review(ReviewRequest::new(repo.path())).unwrap().session().clone();
    service.comment(&id, Anchor::lines(Side::New, "a.txt", 50..=50), "Why?").unwrap();
    let mut rt = window(&service);

    type_text(&mut rt, "n");
    let after_n = lines_in_view(&rt);
    type_text(&mut rt, "p");
    let after_p = lines_in_view(&rt);

    assert_eq!((after_n, after_p), (vec![50], vec![50]));
}

#[test]
fn a_resolved_thread_is_not_visited() {
    let repo = repo_with_a_long_change();
    let service = Arc::new(Service::new());
    let pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    let id = pending.session().clone();
    for line in THREAD_LINES {
        service.comment(&id, Anchor::lines(Side::New, "a.txt", line..=line), "Why?").unwrap();
    }
    service.hand_over_round(&id).unwrap();
    assert!(pending.wait_timeout(Duration::from_secs(5)).is_some(), "the Round wasn't handed over");
    let t2 = ThreadId::new("T2");
    service.reply(repo.path(), &t2, "Because.").unwrap();
    service.resolve(repo.path(), &t2).unwrap();
    service.show(&id, ChangesetView::Full).unwrap();
    let mut rt = window(&service);

    let mut visited = Vec::new();
    for _ in 0..3 {
        type_text(&mut rt, "n");
        visited.push(lines_in_view(&rt));
    }

    assert_eq!(visited, [[5], [95], [5]], "the Resolved Thread was visited");
}

#[test]
fn an_outdated_thread_and_a_general_thread_are_not_visited() {
    let repo = repo_with_a_long_change();
    let (service, id, _rt) = review_with_three_threads(&repo);
    service.comment(&id, Anchor::Review, "In general").unwrap();
    service.comment(&id, Anchor::file("a.txt"), "On the file").unwrap();
    // The 50th line changes under its Thread, which becomes Outdated.
    let mut text: Vec<String> = (1..=100).map(|i| format!("new {i}\n")).collect();
    text[49] = "rewritten\n".into();
    repo.write("a.txt", &text.concat());
    service.reload(&id).unwrap();
    let mut rt = window(&service);

    let mut visited = Vec::new();
    for _ in 0..3 {
        type_text(&mut rt, "n");
        visited.push(lines_in_view(&rt));
    }

    assert_eq!(visited, [[5], [95], [5]], "an Outdated or General Thread was visited");
}

#[test]
fn a_thread_in_a_folded_file_is_gone_to_and_unfolds_it_but_the_viewed_mark_stays() {
    let repo = repo_with_a_long_change();
    let (service, id, mut rt) = review_with_three_threads(&repo);
    press(&mut rt, "viewed-a.txt");
    deliver_window(&mut rt, &service);
    assert!(rt.find("df0.n5").is_none(), "ticking Viewed didn't fold the file");

    type_text(&mut rt, "n");

    assert!(shows(&rt, "df0.n5"), "going to the Thread didn't unfold its file");
    let window = service.window();
    let viewed = &window.sessions.iter().find(|s| s.id == id).unwrap().viewed;
    assert!(viewed.iter().any(|path| path == "a.txt"), "unfolding the file cleared its viewed mark");
}

#[test]
fn after_switching_the_changeset_view_only_threads_on_lines_of_the_view_now_shown_are_visited() {
    let repo = TestRepo::new();
    let old: String = (1..=100).map(|i| format!("old {i}\n")).collect();
    repo.write("a.txt", &old).commit_all("base");
    let first: String = (1..=100).map(|i| if i == 95 { "changed 95\n".into() } else { format!("old {i}\n") }).collect();
    repo.write("a.txt", &first);
    let service = Arc::new(Service::new());
    let pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    let id = pending.session().clone();
    // A Thread on the 95th line, in the first Round's Changeset, and another on the 5th, that
    // only the second Round changes.
    service.comment(&id, Anchor::lines(Side::New, "a.txt", 95..=95), "Why?").unwrap();
    service.hand_over_round(&id).unwrap();
    assert!(pending.wait_timeout(Duration::from_secs(5)).is_some(), "the Round wasn't handed over");
    let second: String = (1..=100)
        .map(|i| match i {
            5 => "changed 5\n".to_owned(),
            95 => "changed 95\n".to_owned(),
            _ => format!("old {i}\n"),
        })
        .collect();
    repo.write("a.txt", &second);
    let _next = service.review(ReviewRequest::new(repo.path())).unwrap();
    service.comment(&id, Anchor::lines(Side::New, "a.txt", 5..=5), "And here?").unwrap();
    service.show(&id, ChangesetView::SinceLastRound).unwrap();
    let mut rt = window(&service);

    let mut visited = Vec::new();
    for _ in 0..2 {
        type_text(&mut rt, "n");
        visited.push((shows(&rt, "df0.n5"), shows(&rt, "df0.n95")));
    }

    assert_eq!(visited, [(true, false), (true, false)], "a Thread outside the view shown was visited");
}

#[test]
fn n_and_p_typed_in_a_composer_are_text_and_move_nothing() {
    let repo = repo_with_a_long_change();
    let (_service, _id, mut rt) = review_with_three_threads(&repo);
    let (x, y) = rt.find("plus").expect("the Changeset offers a Comment on its lines");
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(0.0);
    assert!(rt.find("composer").is_some(), "the comment box didn't open");
    let before = lines_in_view(&rt);

    type_text(&mut rt, "npn");

    assert_eq!(lines_in_view(&rt), before, "a key moved the diff while the composer had focus");
    assert!(rt.find("composer").is_some(), "the composer closed");
}

/// Clicks the element keyed `key` with the pointer, which focuses it, then builds the next frame.
fn click(rt: &mut Runtime<State>, key: &str) {
    let (x, y) = rt.find(key).unwrap_or_else(|| panic!("nothing to click at `{key}`"));
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(0.0);
}

#[test]
fn n_and_p_still_move_after_a_button_was_clicked() {
    let repo = repo_with_a_long_change();
    let (_service, _id, mut rt) = review_with_three_threads(&repo);
    click(&mut rt, "next-thread");
    assert_eq!(lines_in_view(&rt), [5], "the down chevron didn't go to the first Thread");

    type_text(&mut rt, "n");

    assert_eq!(lines_in_view(&rt), [50], "`n` did nothing while the clicked button had focus");
}

#[test]
fn n_and_p_typed_in_a_reply_composer_are_text_and_move_nothing() {
    let repo = repo_with_a_long_change();
    let service = Arc::new(Service::new());
    let pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    let id = pending.session().clone();
    // On the first old line, which the diff shows at its top, where its Reopen button can be pressed.
    service.comment(&id, Anchor::lines(Side::Old, "a.txt", 1..=1), "Why?").unwrap();
    for line in THREAD_LINES {
        service.comment(&id, Anchor::lines(Side::New, "a.txt", line..=line), "Why?").unwrap();
    }
    service.hand_over_round(&id).unwrap();
    assert!(pending.wait_timeout(Duration::from_secs(5)).is_some(), "the Round wasn't handed over");
    // The Round is answered, so it is the human's Turn.
    for thread in ["T1", "T2", "T3", "T4"] {
        service.reply(repo.path(), &ThreadId::new(thread), "Answered.").unwrap();
    }
    service.resolve(repo.path(), &ThreadId::new("T1")).unwrap();
    service.show(&id, ChangesetView::Full).unwrap();
    let mut rt = window(&service);
    press(&mut rt, "reopen-T1");
    deliver_window(&mut rt, &service);
    press(&mut rt, "reply-T1");
    assert!(rt.find("reply-composer").is_some(), "the reply composer didn't open");
    let before = lines_in_view(&rt);

    type_text(&mut rt, "npn");

    assert_eq!(lines_in_view(&rt), before, "a key moved the diff while the reply composer had focus");
    assert!(rt.find("reply-composer").is_some(), "the reply composer closed");
}

#[test]
fn n_and_p_typed_in_the_search_field_are_text_and_move_nothing() {
    let repo = repo_with_a_long_change();
    let (_service, _id, mut rt) = review_with_three_threads(&repo);
    rt.key(KeyEvent { key: Key::Command('f'), mods: Mods { cmd: true, ctrl: !cfg!(target_os = "macos"), ..Mods::default() } });
    rt.frame(0.0);
    assert!(rt.find("search-field").is_some(), "Ctrl+F didn't open the search field");

    // No line says "pnew", but every one says "new": the keys didn't reach the field if it matches.
    type_text(&mut rt, "pnew");

    assert_eq!(texts_within(&rt, "search-count").join(""), "No matches", "`p` or `n` didn't reach the search field");
}

/// A repository whose `README.md` and `a.txt` both changed, `a.txt` over a hundred lines.
fn repo_with_a_readme() -> TestRepo {
    let repo = TestRepo::new();
    let old: String = (1..=100).map(|i| format!("old {i}\n")).collect();
    repo.write("README.md", "# One\n").write("a.txt", &old).commit_all("base");
    let new: String = (1..=100).map(|i| format!("new {i}\n")).collect();
    repo.write("README.md", "# Two\n").write("a.txt", &new);
    repo
}

#[test]
fn n_and_p_do_nothing_while_a_rendered_view_is_open() {
    let repo = repo_with_a_readme();
    let service = Arc::new(Service::new());
    let id = service.review(ReviewRequest::new(repo.path())).unwrap().session().clone();
    for line in THREAD_LINES {
        service.comment(&id, Anchor::lines(Side::New, "a.txt", line..=line), "Why?").unwrap();
    }
    service.mark_viewed(&id, "a.txt", true).unwrap();
    let mut rt = window(&service);
    press(&mut rt, "rendered-README.md");
    assert!(rt.find("rendered-scroll").is_some(), "the Rendered view didn't open");

    // Going to a Thread would unfold its file, which is folded as viewed.
    type_text(&mut rt, "np");
    press(&mut rt, "rendered-close");

    assert!(rt.find("rendered-scroll").is_none(), "the Rendered view didn't close");
    assert!(rt.find("df1.n5").is_none(), "`n` or `p` went to a Thread under the Rendered view");
}

#[test]
fn n_and_p_do_nothing_on_the_inbox() {
    let service = Arc::new(Service::new());
    let mut rt = window(&service);

    type_text(&mut rt, "np");

    assert!(rt.find("session-header").is_none(), "a Thread key opened a Session");
}

/// The tooltip of the element keyed `key`.
fn tooltip(rt: &Runtime<State>, key: &str) -> String {
    let found = rt.elements().into_iter().find(|e| e.key.as_deref() == Some(key));
    found.unwrap_or_else(|| panic!("no element keyed `{key}`")).help.unwrap_or_default()
}

/// Whether the button keyed `key` can be pressed.
fn enabled(rt: &Runtime<State>, key: &str) -> bool {
    rt.elements().into_iter().find(|e| e.key.as_deref() == Some(key)).unwrap_or_else(|| panic!("no `{key}`")).focusable
}

#[test]
fn the_session_header_has_buttons_that_do_what_p_and_n_do() {
    let repo = repo_with_a_long_change();
    let (_service, _id, mut rt) = review_with_three_threads(&repo);

    press(&mut rt, "next-thread");
    assert_eq!(lines_in_view(&rt), [5], "the down chevron didn't go to the first Thread");
    press(&mut rt, "next-thread");
    assert_eq!(lines_in_view(&rt), [50]);
    press(&mut rt, "previous-thread");

    assert_eq!(lines_in_view(&rt), [5], "the up chevron didn't go to the previous Thread");
}

#[test]
fn each_button_has_a_tooltip_naming_what_it_does_and_its_key() {
    let repo = repo_with_a_long_change();
    let (_service, _id, rt) = review_with_three_threads(&repo);

    let (next, previous) = (tooltip(&rt, "next-thread"), tooltip(&rt, "previous-thread"));

    assert_eq!(next, "Next unresolved Thread  n");
    assert_eq!(previous, "Previous unresolved Thread  p");
}

#[test]
fn both_buttons_are_disabled_without_a_thread_to_visit_and_enabled_once_there_is_one() {
    let repo = repo_with_a_long_change();
    let service = Arc::new(Service::new());
    let id = service.review(ReviewRequest::new(repo.path())).unwrap().session().clone();
    let mut rt = window(&service);
    let before = (enabled(&rt, "next-thread"), enabled(&rt, "previous-thread"));

    service.comment(&id, Anchor::lines(Side::New, "a.txt", 5..=5), "Why?").unwrap();
    deliver_window(&mut rt, &service);
    let after = (enabled(&rt, "next-thread"), enabled(&rt, "previous-thread"));

    assert_eq!((before, after), ((false, false), (true, true)));
}

const PR_URL: &str = "https://forge.test/acme/web/pull/7";

/// A Forge with one Pull Request, `acme/web#7`, whose `login.rs` goes from one line to three.
struct FakeForge;

impl ForgeAdapter for FakeForge {
    fn host(&self) -> &str {
        "forge.test"
    }

    fn recognise(&self, url: &str) -> Option<PullRequestRef> {
        url.starts_with(PR_URL).then(|| PullRequestRef {
            host: "forge.test".into(),
            repo: "acme/web".into(),
            number: 7,
        })
    }

    fn pull_request(&self, id: &PullRequestRef) -> Result<PullRequest, ForgeError> {
        Ok(PullRequest {
            id: id.clone(),
            url: PR_URL.into(),
            title: "Add SSO login".into(),
            author: "maria".into(),
            head_branch: "sso".into(),
            head_sha: "headsha".into(),
            base_sha: "basesha".into(),
        })
    }

    fn changeset(&self, _: &PullRequest) -> Result<Changeset, ForgeError> {
        let login = FileChange::text("login.rs", Some("fn login() {}\n"), Some("fn login() {\n    sso();\n}\n"));
        Ok(Changeset { base: "basesha".into(), files: vec![login] })
    }

    fn threads(&self, _: &PullRequest) -> Result<Vec<RemoteThread>, ForgeError> {
        Ok(Vec::new())
    }

    fn viewer(&self) -> Result<String, ForgeError> {
        Ok("me".into())
    }

    fn verdicts(&self, _: &PullRequest, _: bool) -> Result<Vec<Verdict>, ForgeError> {
        Ok(Verdict::ALL.to_vec())
    }

    fn publish(&self, _: &PullRequest, _: &Review) -> Result<String, ForgeError> {
        Ok(format!("{PR_URL}#pullrequestreview-1"))
    }
}

#[test]
fn a_pull_request_sessions_header_has_the_buttons_too_and_they_visit_its_threads() {
    let service = Arc::new(Service::new().with_forges(vec![Box::new(FakeForge)]));
    let id = service.open_pull_request(PR_URL).unwrap();
    let mut rt = window(&service);
    assert!(!enabled(&rt, "next-thread"), "the button was enabled with no Thread");
    service.comment(&id, Anchor::lines(Side::New, "login.rs", 2..=2), "Why sso?").unwrap();
    deliver_window(&mut rt, &service);

    press(&mut rt, "next-thread");

    assert!(enabled(&rt, "next-thread") && enabled(&rt, "previous-thread"), "the buttons stayed disabled");
    assert!(shows(&rt, "df0.n2"), "the button didn't go to the Thread");
}
