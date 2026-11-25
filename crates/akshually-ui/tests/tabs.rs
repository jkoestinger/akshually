//! UI: the Window's tab strip, in Rune's headless `Runtime` against real Sessions in the core
//! service: picking what the Window displays (a pick shows once the service took it, and one it
//! refused changes nothing), and ending a Session from its tab's ✕ (#86).

use std::sync::Arc;
use std::time::Duration;

use rune::accesskit::Action;
use rune::{Env, Runtime};

use akshually_core::forge::{ForgeAdapter, ForgeError, PullRequest, PullRequestRef, RemoteThread, Review};
use akshually_core::{
    Anchor, Changeset, FileChange, Outcome, PendingReview, ReviewRequest, Service, SessionId, Verdict,
};
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

/// Presses the element keyed `key` as an assistive client would.
fn press(rt: &mut Runtime<State>, key: &str) {
    assert!(rt.accessibility_action(key, Action::Click, None), "nothing to press at `{key}`");
    rt.frame(0.0);
}

#[test]
fn picking_a_tab_shows_its_session() {
    let (repo_a, repo_b) = (repo_with_one_edit(), repo_with_one_edit());
    let service = Arc::new(Service::new());
    let _a = service.review(ReviewRequest::new(repo_a.path())).unwrap();
    let b = service.review(ReviewRequest::new(repo_b.path())).unwrap();
    let mut rt = window(&service);

    press(&mut rt, &format!("t-{}", b.session()));

    assert!(rt.find(&format!("diff-{}", b.session())).is_some(), "the picked tab's Session doesn't show");
}

#[test]
fn picking_the_tab_of_a_session_that_just_ended_leaves_the_view_as_it_was() {
    let (repo_a, repo_b) = (repo_with_one_edit(), repo_with_one_edit());
    let service = Arc::new(Service::new());
    let shown = service.review(ReviewRequest::new(repo_a.path())).unwrap();
    let ended = service.review(ReviewRequest::new(repo_b.path())).unwrap();
    let mut rt = window(&service);
    // Ended, but the window hasn't had the update: its tab is still there to pick.
    service.approve(ended.session()).unwrap();

    press(&mut rt, &format!("t-{}", ended.session()));

    assert!(rt.find(&format!("diff-{}", shown.session())).is_some(), "the view moved off the Session it showed");
    assert!(rt.find(&format!("diff-{}", ended.session())).is_none(), "the ended Session shows");
}

/// The centre of the ✕ on the tab keyed `t-{tab}`, if it has one: the strip labels it for
/// assistive clients but gives it no key of its own.
fn close_button(rt: &Runtime<State>, tab: &str) -> Option<(f32, f32)> {
    let within = format!("/t-{tab}/");
    rt.elements()
        .into_iter()
        .find(|e| e.path.contains(&within) && e.label.as_deref() == Some("Close tab") && !e.exiting)
        .map(|e| (e.rect[0] + e.rect[2] / 2.0, e.rect[1] + e.rect[3] / 2.0))
}

/// Presses the ✕ on the tab of the Session `id`, as a pointer does, there being no key for an
/// assistive client to press it by.
fn press_close(rt: &mut Runtime<State>, id: &SessionId) {
    let (x, y) = close_button(rt, id.as_str()).unwrap_or_else(|| panic!("the tab of {id} has no ✕"));
    rt.pointer_move(x, y);
    rt.frame(0.0);
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(0.0);
}

#[test]
fn every_session_tab_has_a_close_button_and_the_inbox_tab_has_none() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let pending = service.review(ReviewRequest::new(repo.path())).unwrap();

    let rt = window(&service);

    assert!(close_button(&rt, pending.session().as_str()).is_some(), "the Session tab has no ✕");
    assert!(close_button(&rt, "inbox").is_none(), "the Inbox tab has a ✕");
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

/// Local work on branch `feature` that an agent co-reviews: it has drafted on it and waits in
/// `review` for the human.
fn local_co_review(service: &Service) -> (TestRepo, PendingReview) {
    let repo = TestRepo::new();
    repo.write("lib.rs", "fn a() {}\n").commit_all("base");
    repo.branch("feature").write("lib.rs", "fn a() {}\nfn b() {}\n").commit_all("add b");
    service.draft(repo.path(), Anchor::Review, "Small.").unwrap();
    let pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    (repo, pending)
}

#[test]
fn closing_the_tab_of_a_local_co_review_ends_it_as_closed_without_asking() {
    let service = Arc::new(Service::new());
    let (_repo, pending) = local_co_review(&service);
    let id = pending.session().clone();
    let mut rt = window(&service);

    press_close(&mut rt, &id);
    deliver_window(&mut rt, &service);

    assert_eq!(pending.wait_timeout(Duration::from_secs(5)), Some(Outcome::Closed));
    assert!(rt.find(&format!("t-{id}")).is_none(), "the ended Session's tab is still there");
    assert!(rt.find("inbox").is_some(), "the Window doesn't show the Inbox");
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
fn closing_the_tab_of_local_work_asks_before_abandoning_it() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    let mut rt = window(&service);

    press_close(&mut rt, pending.session());

    let asked = texts_within(&rt, "end-confirm");
    assert!(asked.contains(&"Abandon this Session? The agent is told to stop.".to_owned()), "asked {asked:?}");
    assert!(service.session(pending.session()).unwrap().ended.is_none(), "it ended before the human confirmed");
}

#[test]
fn closing_a_tab_that_isnt_shown_shows_its_session_with_the_question() {
    let (repo_a, repo_b) = (repo_with_one_edit(), repo_with_one_edit());
    let service = Arc::new(Service::new());
    let shown = service.review(ReviewRequest::new(repo_a.path())).unwrap();
    let other = service.review(ReviewRequest::new(repo_b.path())).unwrap();
    let mut rt = window(&service);
    assert!(rt.find(&format!("diff-{}", shown.session())).is_some(), "the first Session to ask isn't shown");

    press_close(&mut rt, other.session());

    assert!(rt.find(&format!("diff-{}", other.session())).is_some(), "the closed tab's Session doesn't show");
    assert!(rt.find("end-confirm").is_some(), "the question isn't asked");
}

#[test]
fn confirming_the_close_of_local_works_tab_abandons_it() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    let mut rt = window(&service);
    press_close(&mut rt, pending.session());

    press(&mut rt, "end-confirm-end");

    assert_eq!(pending.wait_timeout(Duration::from_secs(5)), Some(Outcome::Abandoned { pull_requests: vec![] }));
}

const PR_URL: &str = "https://forge.test/acme/web/pull/7";

/// A Forge with one Pull Request, `acme/web#7`, changing `login.rs`.
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
            base_branch: "main".into(),
            from_fork: false,
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

/// A Pull Request Session the human opened and wrote a Draft in, which isn't Published yet.
fn pull_request_with_a_draft() -> (Arc<Service>, SessionId) {
    let service = Arc::new(Service::new().with_forges(vec![Box::new(FakeForge)]));
    let id = service.open_pull_request(PR_URL).unwrap();
    service.comment(&id, Anchor::Review, "Looks risky.").unwrap();
    (service, id)
}

#[test]
fn closing_the_tab_of_a_pull_request_with_drafts_asks_before_discarding_them() {
    let (service, id) = pull_request_with_a_draft();
    let mut rt = window(&service);

    press_close(&mut rt, &id);

    let asked = texts_within(&rt, "end-confirm");
    assert!(
        asked.contains(&"Close this Session? Your unpublished changes are discarded.".to_owned()),
        "asked {asked:?}"
    );
    assert!(asked.contains(&"Close Session".to_owned()), "no Close Session button in {asked:?}");
    assert!(service.session(&id).is_some(), "it closed before the human confirmed");
}

#[test]
fn closing_the_tab_of_a_pull_request_with_a_co_reviewer_says_it_is_told_to_stop() {
    let service = Arc::new(Service::new().with_forges(vec![Box::new(FakeForge)]));
    let pending = service.review(ReviewRequest::pull_request(PR_URL)).unwrap();
    let mut rt = window(&service);

    press_close(&mut rt, pending.session());

    let asked = texts_within(&rt, "end-confirm");
    let question =
        "Close this Session? Your unpublished changes are discarded. The co-Reviewer is told to stop.".to_owned();
    assert!(asked.contains(&question), "asked {asked:?}");
}

#[test]
fn confirming_the_close_of_a_pull_requests_tab_closes_its_session() {
    let (service, id) = pull_request_with_a_draft();
    let mut rt = window(&service);
    press_close(&mut rt, &id);

    press(&mut rt, "end-confirm-end");
    deliver_window(&mut rt, &service);

    assert!(service.session(&id).is_none(), "the Session is still open");
    assert!(rt.find(&format!("t-{id}")).is_none(), "the closed Session's tab is still there");
}

#[test]
fn cancelling_the_close_of_a_pull_requests_tab_keeps_its_drafts() {
    let (service, id) = pull_request_with_a_draft();
    let mut rt = window(&service);
    press_close(&mut rt, &id);

    press(&mut rt, "end-confirm-cancel");

    assert!(rt.find("end-confirm").is_none(), "the question is still asked");
    assert_eq!(service.session(&id).map(|s| s.drafts()), Some(1), "the Session or its Draft went");
}

#[test]
fn a_pull_request_sessions_header_has_no_close_button() {
    let (service, _id) = pull_request_with_a_draft();

    let rt = window(&service);

    let header = texts_within(&rt, "session-header");
    assert!(header.contains(&"Publish…".to_owned()), "not the Pull Request's header: {header:?}");
    assert!(!header.contains(&"Close".to_owned()), "a Close button in {header:?}");
}

#[test]
fn a_local_co_reviews_header_has_no_close_button() {
    let service = Arc::new(Service::new());
    let (_repo, _pending) = local_co_review(&service);

    let rt = window(&service);

    let header = texts_within(&rt, "session-header");
    assert!(header.contains(&"feature".to_owned()), "not the co-review's header: {header:?}");
    assert!(!header.contains(&"Close".to_owned()), "a Close button in {header:?}");
}

#[test]
fn local_works_header_offers_abandon_instead_of_close() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let _pending = service.review(ReviewRequest::new(repo.path())).unwrap();

    let rt = window(&service);

    let header = texts_within(&rt, "session-header");
    assert!(header.contains(&"Abandon".to_owned()), "no Abandon button in {header:?}");
    assert!(!header.contains(&"Close".to_owned()), "a Close button in {header:?}");
}

#[test]
fn cancelling_the_close_of_local_works_tab_leaves_it_open() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    let mut rt = window(&service);
    press_close(&mut rt, pending.session());

    press(&mut rt, "end-confirm-cancel");

    assert!(rt.find("end-confirm").is_none(), "the question is still asked");
    assert!(service.session(pending.session()).unwrap().ended.is_none(), "the Session ended");
}
