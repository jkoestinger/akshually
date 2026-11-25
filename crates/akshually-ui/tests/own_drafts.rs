//! UI: the Window's Edit, Save, Cancel and Discard on the human's own Drafts in Rune's headless
//! `Runtime`, against real Sessions in the core service (#108).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rune::Runtime;
use rune::accesskit::Action;

use akshually_core::forge::{
    ForgeAdapter, ForgeError, PullRequest, PullRequestRef, RemoteAnchor, RemoteComment, RemoteThread, Review,
};
use akshually_core::{
    Anchor, Changeset, CommentRef, FileChange, ReviewRequest, Service, SessionId, Side, ThreadId, ThreadRef, Verdict,
};
use akshually_ui::State;

mod common;

use common::{repo_with_one_edit, window};

/// Hands the service's Window to the window, as its subscriber does, and builds the next frame.
fn deliver_window(rt: &mut Runtime<State>, service: &Service) {
    let window = service.window();
    rt.update_if(move |s: &mut State, _| {
        s.apply(window);
        true
    });
    rt.frame(0.0);
}

/// Presses the element keyed `key` as an assistive client would, then hands the window what the
/// service did.
fn press(rt: &mut Runtime<State>, service: &Service, key: &str) {
    assert!(rt.accessibility_action(key, Action::Click, None), "nothing to press at `{key}`");
    rt.frame(0.0);
    deliver_window(rt, service);
}

/// Every text the window draws.
fn texts(rt: &Runtime<State>) -> Vec<String> {
    rt.elements().into_iter().filter(|e| !e.exiting).filter_map(|e| e.text).collect()
}

fn shows(rt: &Runtime<State>, text: &str) -> bool {
    texts(rt).iter().any(|t| t.contains(text))
}

/// The Session of `repo` with the human's Comment `T1` on its one line, still a Draft.
fn draft(service: &Service, repo: &akshually_testkit::TestRepo) -> SessionId {
    let id = service.review(ReviewRequest::new(repo.path())).unwrap().session().clone();
    service.comment(&id, Anchor::lines(Side::New, "a.txt", 1..=1), "Why two?").unwrap();
    id
}

/// `draft`'s Thread, once the agent received it and answered, with the human's Draft reply.
fn reply_draft(service: &Service, repo: &akshually_testkit::TestRepo) -> SessionId {
    let pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    let id = pending.session().clone();
    service.comment(&id, Anchor::lines(Side::New, "a.txt", 1..=1), "Why two?").unwrap();
    service.hand_over_round(&id).unwrap();
    assert!(pending.wait_timeout(Duration::from_secs(5)).is_some(), "the Round wasn't handed over");
    let t1 = ThreadId::new("T1");
    service.reply(repo.path(), &t1, "Because one was wrong.").unwrap();
    service.reply_in_thread(&ThreadRef::new(&id, &t1), "Thanks, but why?").unwrap();
    id
}

fn body(service: &Service, id: &SessionId, comment: usize) -> String {
    service.session(id).unwrap().threads[0].comments[comment].body.clone()
}

#[test]
fn edit_opens_an_editor_on_the_drafts_text() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    draft(&service, &repo);
    let mut rt = window(&service);

    press(&mut rt, &service, "edit-T1-0");

    assert!(rt.find("edit-composer").is_some(), "the editor didn't open");
    assert!(shows(&rt, "Why two?"), "the editor doesn't hold the Draft's text");
}

/// The window on `draft`'s Session with its Draft edited to say "Why two? Really?" and saved.
fn after_saving_an_edit(service: &Arc<Service>, repo: &akshually_testkit::TestRepo) -> (SessionId, Runtime<State>) {
    let id = draft(service, repo);
    let mut rt = window(service);
    press(&mut rt, service, "edit-T1-0");
    rt.paste(" Really?");
    rt.frame(0.0);
    press(&mut rt, service, "save-edit");
    (id, rt)
}

#[test]
fn saving_an_edit_closes_the_editor() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());

    let (_, rt) = after_saving_an_edit(&service, &repo);

    assert!(rt.find("edit-composer").is_none(), "the editor stayed open");
}

#[test]
fn saving_an_edit_changes_the_drafts_text_in_the_session() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());

    let (id, _) = after_saving_an_edit(&service, &repo);

    assert!(body(&service, &id, 0).contains("Really?"), "{}", body(&service, &id, 0));
}

#[test]
fn saving_an_edit_shows_the_new_text_in_the_card() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());

    let (_, rt) = after_saving_an_edit(&service, &repo);

    assert!(shows(&rt, "Really?"), "the card doesn't show the new text");
}

#[test]
fn an_edited_draft_is_still_a_draft() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());

    let (id, _) = after_saving_an_edit(&service, &repo);

    assert!(service.session(&id).unwrap().threads[0].comments[0].is_draft());
}

#[test]
fn cancel_leaves_the_draft_as_it_was() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let id = draft(&service, &repo);
    let mut rt = window(&service);
    press(&mut rt, &service, "edit-T1-0");
    rt.paste(" Really?");
    rt.frame(0.0);

    press(&mut rt, &service, "cancel-edit");

    assert!(rt.find("edit-composer").is_none(), "the editor stayed open");
    assert_eq!(body(&service, &id, 0), "Why two?");
    assert!(!shows(&rt, "Really?"), "the card shows what was cancelled");
}

#[test]
fn discard_removes_the_thread_that_the_draft_opened() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let id = draft(&service, &repo);
    let mut rt = window(&service);

    press(&mut rt, &service, "discard-T1-0");

    assert!(service.session(&id).unwrap().threads.is_empty(), "the Thread is still there");
    assert!(!shows(&rt, "Why two?"), "the card is still shown");
}

#[test]
fn discard_removes_a_draft_reply_and_leaves_the_rest_of_the_thread() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let id = reply_draft(&service, &repo);
    let mut rt = window(&service);
    assert!(shows(&rt, "Thanks, but why?"), "the Draft reply isn't shown");
    assert!(rt.find("edit-T1-0").is_none(), "the Comment a Round carried offers Edit");

    press(&mut rt, &service, "discard-T1-2");

    assert_eq!(service.session(&id).unwrap().threads[0].comments.len(), 2);
    assert!(!shows(&rt, "Thanks, but why?"), "the reply is still shown");
    assert!(shows(&rt, "Because one was wrong."), "the agent's reply went too");
}

#[test]
fn a_draft_reply_is_edited_in_place_under_the_comments_before_it() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let id = reply_draft(&service, &repo);
    let mut rt = window(&service);

    press(&mut rt, &service, "edit-T1-2");
    rt.paste(" Please.");
    rt.frame(0.0);
    press(&mut rt, &service, "save-edit");

    assert!(body(&service, &id, 2).contains("Please."), "{}", body(&service, &id, 2));
    assert_eq!(body(&service, &id, 1), "Because one was wrong.");
    assert!(shows(&rt, "Please."));
}

#[test]
fn discarding_an_earlier_draft_moves_the_editor_on_a_later_one_up_with_it() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let id = reply_draft(&service, &repo);
    service.reply_in_thread(&ThreadRef::new(&id, &ThreadId::new("T1")), "And one more.").unwrap();
    let mut rt = window(&service);
    press(&mut rt, &service, "edit-T1-3");

    press(&mut rt, &service, "discard-T1-2");
    rt.paste(" Please.");
    rt.frame(0.0);
    press(&mut rt, &service, "save-edit");

    assert_eq!(body(&service, &id, 2), "And one more. Please.");
}

#[test]
fn a_draft_reply_with_a_suggestion_block_is_saved_as_typed() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let id = reply_draft(&service, &repo);
    let t1 = ThreadRef::new(&id, &ThreadId::new("T1"));
    service.discard_own_draft(&CommentRef::new(&t1, 2)).unwrap();
    service.reply_in_thread(&t1, "Try:\n```suggestion\ntwo\n```").unwrap();
    let mut rt = window(&service);
    press(&mut rt, &service, "edit-T1-2");

    rt.paste("\nPlease.");
    rt.frame(0.0);
    press(&mut rt, &service, "save-edit");

    assert!(rt.find("edit-composer").is_none(), "the editor stayed open");
    assert_eq!(body(&service, &id, 2), "Try:\n```suggestion\ntwo\n```\nPlease.");
}

/// `reply_draft`'s Thread, Resolved by the human with its Draft reply still unsent.
fn resolved(service: &Arc<Service>, repo: &akshually_testkit::TestRepo) -> SessionId {
    let id = reply_draft(service, repo);
    service.resolve_thread(&ThreadRef::new(&id, &ThreadId::new("T1"))).unwrap();
    id
}

#[test]
fn an_expanded_resolved_thread_offers_edit_on_its_draft_reply() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let id = resolved(&service, &repo);
    let mut rt = window(&service);
    press(&mut rt, &service, "toggle-T1");

    press(&mut rt, &service, "edit-T1-2");
    rt.paste(" Please.");
    rt.frame(0.0);
    press(&mut rt, &service, "save-edit");

    assert!(body(&service, &id, 2).contains("Please."), "{}", body(&service, &id, 2));
}

#[test]
fn discarding_a_draft_reply_in_a_resolved_thread_leaves_it_resolved() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let id = resolved(&service, &repo);
    let mut rt = window(&service);
    press(&mut rt, &service, "toggle-T1");

    press(&mut rt, &service, "discard-T1-2");

    let thread = service.session(&id).unwrap().threads[0].clone();
    assert_eq!((thread.comments.len(), thread.resolved.is_some()), (2, true));
}

/// A Draft `T1` on the one line of `a.txt`, which the file then rewrites: the Thread is Outdated.
fn outdated(service: &Arc<Service>, repo: &akshually_testkit::TestRepo) -> SessionId {
    let id = draft(service, repo);
    repo.write("a.txt", "three\n");
    service.reload(&id).unwrap();
    assert!(service.session(&id).unwrap().threads[0].outdated, "the Thread didn't go Outdated");
    id
}

#[test]
fn an_expanded_outdated_thread_offers_edit_on_its_draft() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let id = outdated(&service, &repo);
    let mut rt = window(&service);
    press(&mut rt, &service, "toggle-T1");

    press(&mut rt, &service, "edit-T1-0");
    rt.paste(" Really?");
    rt.frame(0.0);
    press(&mut rt, &service, "save-edit");

    assert!(body(&service, &id, 0).contains("Really?"), "{}", body(&service, &id, 0));
}

#[test]
fn an_expanded_outdated_thread_offers_discard_on_its_draft() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let id = outdated(&service, &repo);
    let mut rt = window(&service);
    press(&mut rt, &service, "toggle-T1");

    press(&mut rt, &service, "discard-T1-0");

    assert!(service.session(&id).unwrap().threads.is_empty(), "the Thread is still there");
}

#[test]
fn the_editor_of_local_work_says_the_draft_goes_out_with_the_round() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    draft(&service, &repo);
    let mut rt = window(&service);

    press(&mut rt, &service, "edit-T1-0");

    assert!(shows(&rt, "until you hand over the Round"), "{:?}", texts(&rt));
}

#[test]
fn an_editor_open_when_the_round_goes_out_is_closed_when_the_human_takes_the_turn_back() {
    let repo = repo_with_one_edit();
    let service = Arc::new(Service::new());
    let pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    let id = pending.session().clone();
    service.comment(&id, Anchor::lines(Side::New, "a.txt", 1..=1), "Why two?").unwrap();
    let mut rt = window(&service);
    press(&mut rt, &service, "edit-T1-0");
    drop(pending);
    service.hand_over_round(&id).unwrap();
    deliver_window(&mut rt, &service);

    service.take_turn_back(&id).unwrap();
    deliver_window(&mut rt, &service);

    assert!(rt.find("edit-composer").is_none(), "the editor came back with the Draft");
}

const PR_URL: &str = "https://forge.test/acme/web/pull/7";

/// A Forge with one Pull Request, `acme/web#7`, whose Thread `RT_1` has `lee`'s question and
/// as many more replies of `maria` as `replies` says, which the test raises between reloads.
struct FakeForge {
    replies: Arc<AtomicUsize>,
}

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
            ..PullRequest::default()
        })
    }

    fn changeset(&self, _: &PullRequest) -> Result<Changeset, ForgeError> {
        let login = FileChange::text("login.rs", Some("fn login() {}\n"), Some("fn login() {\n    sso();\n}\n"));
        Ok(Changeset { base: "basesha".into(), files: vec![login] })
    }

    fn threads(&self, _: &PullRequest) -> Result<Vec<RemoteThread>, ForgeError> {
        let comment = |author: &str, body: String| RemoteComment { author: author.into(), body };
        let mut comments = vec![comment("lee", "Which SSO provider?".into())];
        comments.extend((0..self.replies.load(Ordering::SeqCst)).map(|n| comment("maria", format!("Reply {n}."))));
        Ok(vec![RemoteThread {
            id: "RT_1".into(),
            anchor: RemoteAnchor::Lines { path: "login.rs".into(), side: Side::New, start: 2, end: 2 },
            outdated: false,
            resolved_by: None,
            diff_hunk: String::new(),
            comments,
        }])
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

/// The Pull Request Session with two Draft replies in `RT_1`, "Draft A." and "Draft B.", shown
/// in a window; `replies` raises the number of Forge replies a reload brings.
fn pull_request_with_two_draft_replies() -> (Arc<Service>, SessionId, Arc<AtomicUsize>, Runtime<State>) {
    let replies = Arc::new(AtomicUsize::new(0));
    let service = Arc::new(Service::new().with_forges(vec![Box::new(FakeForge { replies: replies.clone() })]));
    let id = service.open_pull_request(PR_URL).unwrap();
    let t1 = ThreadRef::new(&id, &ThreadId::new("T1"));
    service.reply_in_thread(&t1, "Draft A.").unwrap();
    service.reply_in_thread(&t1, "Draft B.").unwrap();
    let rt = window(&service);
    (service, id, replies, rt)
}

fn bodies(service: &Service, id: &SessionId) -> Vec<String> {
    service.session(id).unwrap().threads[0].comments.iter().map(|c| c.body.clone()).collect()
}

#[test]
fn the_editor_of_a_pull_request_says_the_draft_goes_out_with_publish() {
    let (service, _, _, mut rt) = pull_request_with_two_draft_replies();

    press(&mut rt, &service, "edit-T1-2");

    assert!(shows(&rt, "until you Publish"), "{:?}", texts(&rt));
}

/// `pull_request_with_two_draft_replies`, the editor opened on "Draft B.", then a reload that
/// brings one more Forge reply, which puts both Drafts down one place, and " Please." typed.
fn editing_draft_b_across_a_reload() -> (Arc<Service>, SessionId, Runtime<State>) {
    let (service, id, replies, mut rt) = pull_request_with_two_draft_replies();
    press(&mut rt, &service, "edit-T1-2");
    replies.store(1, Ordering::SeqCst);
    service.reload(&id).unwrap();
    deliver_window(&mut rt, &service);
    rt.paste(" Please.");
    rt.frame(0.0);
    (service, id, rt)
}

#[test]
fn a_reload_that_shifts_the_drafts_down_does_not_send_the_save_to_the_draft_above() {
    let (service, id, mut rt) = editing_draft_b_across_a_reload();

    press(&mut rt, &service, "save-edit");

    let bodies = bodies(&service, &id);
    assert_eq!(bodies[2], "Draft A.", "{bodies:?}");
}

#[test]
fn a_reload_that_shifts_the_drafts_down_leaves_the_editor_on_the_draft_it_was_opened_on() {
    let (service, id, mut rt) = editing_draft_b_across_a_reload();

    press(&mut rt, &service, "save-edit");

    let bodies = bodies(&service, &id);
    assert_eq!(bodies[3], "Draft B. Please.", "{bodies:?}");
}
