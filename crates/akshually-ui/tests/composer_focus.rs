//! UI: the Window's Comment fields in Rune's headless `Runtime`, against real Sessions in the core
//! service: each takes keyboard focus as it opens (#99), the composer of a new Thread, the reply
//! composer and the editor of a co-Reviewer's Draft.

use std::sync::Arc;

use rune::accesskit::Action;
use rune::{Env, Runtime};

use akshually_core::forge::{ForgeAdapter, ForgeError, PullRequest, PullRequestRef, RemoteThread, Review};
use akshually_core::{Anchor, Changeset, FileChange, Hunk, ReviewRequest, Service, SessionId, Side, ThreadId, Verdict};
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

/// Presses the element at (`x`, `y`) as a pointer does.
fn click_at(rt: &mut Runtime<State>, (x, y): (f32, f32)) {
    rt.pointer_move(x, y);
    rt.frame(0.0);
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(0.0);
}

/// Presses the button labelled `label`, which has no key to press it by.
fn press_labelled(rt: &mut Runtime<State>, label: &str) {
    let at = rt
        .elements()
        .into_iter()
        .find(|e| e.label.as_deref() == Some(label) && !e.exiting)
        .map(|e| (e.rect[0] + e.rect[2] / 2.0, e.rect[1] + e.rect[3] / 2.0))
        .unwrap_or_else(|| panic!("no button labelled `{label}`"));
    click_at(rt, at);
}

/// Whether the element with keyboard focus lies inside the element keyed `key`.
fn focus_is_within(rt: &Runtime<State>, key: &str) -> bool {
    let within = format!("/{key}/");
    rt.elements().into_iter().any(|e| e.focused && format!("{}/", e.path).contains(&within))
}

/// The texts drawn inside the element keyed `key`, in tree order.
fn texts_within(rt: &Runtime<State>, key: &str) -> Vec<String> {
    let within = format!("/{key}/");
    rt.elements().into_iter().filter(|e| format!("{}/", e.path).contains(&within)).filter_map(|e| e.text).collect()
}

#[test]
fn opening_the_composer_on_a_line_focuses_its_text_area() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let _pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    let mut rt = window(&service);
    let plus = rt.find("plus").expect("the Changeset offers a Comment on its lines");

    click_at(&mut rt, plus);

    assert!(rt.find("composer").is_some(), "the composer didn't open");
    assert!(focus_is_within(&rt, "composer"), "the composer's text area has no focus");
}

#[test]
fn opening_the_composer_on_a_whole_file_focuses_its_text_area() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let _pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    let mut rt = window(&service);

    press_labelled(&mut rt, "Comment on file");

    assert!(rt.find("composer").is_some(), "the composer didn't open");
    assert!(focus_is_within(&rt, "composer"), "the composer's text area has no focus");
}

#[test]
fn opening_the_composer_on_the_whole_review_focuses_its_text_area() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let _pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    let mut rt = window(&service);

    press_labelled(&mut rt, "Comment on Review");

    assert!(rt.find("composer").is_some(), "the composer didn't open");
    assert!(focus_is_within(&rt, "composer"), "the composer's text area has no focus");
}

/// The Session of `repo`'s first Round: the human's Comment `T1` on its one line, which the
/// agent answered and resolved; reopened, it offers a reply.
fn reopened_thread(repo: &TestRepo, service: &Service) -> SessionId {
    let pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    let id = pending.session().clone();
    service.comment(&id, Anchor::lines(Side::New, "a.txt", 1..=1), "Why two?").unwrap();
    service.hand_over_round(&id).unwrap();
    assert!(pending.wait_timeout(std::time::Duration::from_secs(5)).is_some(), "the Round wasn't handed over");
    let t1 = ThreadId::new("T1");
    service.reply(repo.path(), &t1, "Because one was wrong.").unwrap();
    service.resolve(repo.path(), &t1).unwrap();
    id
}

#[test]
fn opening_the_reply_composer_focuses_its_text_area() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let _id = reopened_thread(&repo, &service);
    let mut rt = window(&service);
    press(&mut rt, "reopen-T1");
    deliver_window(&mut rt, &service);

    press(&mut rt, "reply-T1");

    assert!(rt.find("reply-composer").is_some(), "the reply composer didn't open");
    assert!(focus_is_within(&rt, "reply-composer"), "the reply composer's text area has no focus");
}

#[test]
fn opening_the_editor_of_a_co_reviewers_draft_focuses_its_text_area_with_the_drafts_text() {
    let service = Arc::new(Service::new());
    let repo = TestRepo::new();
    repo.write("lib.rs", "fn a() {}\n").commit_all("base");
    repo.branch("feature").write("lib.rs", "fn a() {}\nfn b() {}\n").commit_all("add b");
    service.draft(repo.path(), Anchor::Review, "Small.").unwrap();
    let _pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    let mut rt = window(&service);

    press_labelled(&mut rt, "Edit");

    assert!(texts_within(&rt, "edit-composer").iter().any(|t| t.contains("Small.")), "the Draft's text isn't in it");
    assert!(focus_is_within(&rt, "edit-composer"), "the editor's text area has no focus");
}

#[test]
fn opening_a_field_while_another_is_open_focuses_the_one_just_opened() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let _pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    let mut rt = window(&service);
    press_labelled(&mut rt, "Comment on Review");

    press_labelled(&mut rt, "Comment on file");

    assert!(focus_is_within(&rt, "composer"), "the composer moved to the file has no focus");
}

#[test]
fn opening_the_composer_while_a_reply_is_open_focuses_the_composer() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let _id = reopened_thread(&repo, &service);
    let mut rt = window(&service);
    press(&mut rt, "reopen-T1");
    deliver_window(&mut rt, &service);
    press(&mut rt, "reply-T1");

    press_labelled(&mut rt, "Comment on Review");

    assert!(focus_is_within(&rt, "composer"), "the composer just opened has no focus");
}

const PR_URL: &str = "https://forge.test/acme/web/pull/7";

/// A Forge with one Pull Request that changes `login.rs`'s line 2 and takes line Comments there
/// only, so a Comment on its line 1 is refused.
struct StrictForge;

impl ForgeAdapter for StrictForge {
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
        let mut login = FileChange::text("login.rs", Some("a\nb\nc\n"), Some("a\nB\nc\n"));
        login.hunks = Some(vec![Hunk { old_start: 2, old_lines: 1, new_start: 2, new_lines: 1 }]);
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
fn moving_a_refused_composer_to_its_file_keeps_its_text_and_focuses_its_text_area() {
    let service = Arc::new(Service::new().with_forges(vec![Box::new(StrictForge)]));
    service.open_pull_request(PR_URL).unwrap();
    let mut rt = window(&service);
    // The "+" of the first line, which the Forge's one hunk doesn't hold.
    let plus = rt.find("plus").expect("the Changeset offers a Comment on its lines");
    click_at(&mut rt, plus);
    assert!(
        texts_within(&rt, "composer").iter().any(|t| t.contains("Comment on the file")),
        "the Forge didn't refuse the lines"
    );
    rt.paste("keep me");
    rt.frame(0.0);
    press_labelled(&mut rt, "Comment on the file");

    assert!(texts_within(&rt, "composer").iter().any(|t| t.contains("keep me")), "the composer lost its text");
    assert!(focus_is_within(&rt, "composer"), "the composer moved to the file has no focus");
}
