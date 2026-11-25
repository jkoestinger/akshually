//! Seam A: the human editing and discarding their own Drafts in the core review service, on
//! local work and on a Pull Request with a fake Forge adapter (#108).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use akshually_core::forge::{
    ForgeAdapter, ForgeError, PullRequest, PullRequestRef, RemoteAnchor, RemoteComment, RemoteThread, Review,
};
use akshually_core::{
    Anchor, Changeset, CommentRef, Error, FileChange, NewComment, Outcome, Party, ReviewRequest, Service, SessionId,
    Side, ThreadId, ThreadRef, Verdict,
};
use akshually_testkit::{TestDirs, TestRepo};

const URL: &str = "https://forge.test/acme/web/pull/7";

/// A Forge with one Pull Request, `acme/web#7`, that has the Thread `RT_1` and records each
/// review published.
#[derive(Default)]
struct FakeForge {
    published: Arc<Mutex<Vec<Review>>>,
}

fn pr_ref() -> PullRequestRef {
    PullRequestRef { host: "forge.test".into(), repo: "acme/web".into(), number: 7 }
}

impl ForgeAdapter for FakeForge {
    fn host(&self) -> &str {
        "forge.test"
    }

    fn recognise(&self, url: &str) -> Option<PullRequestRef> {
        url.starts_with(URL).then(pr_ref)
    }

    fn pull_request(&self, _: &PullRequestRef) -> Result<PullRequest, ForgeError> {
        Ok(PullRequest {
            id: pr_ref(),
            url: URL.into(),
            title: "Add SSO login".into(),
            author: "maria".into(),
            head_branch: "sso".into(),
            head_sha: "headsha".into(),
            base_sha: "basesha".into(),
            base_branch: "main".into(),
            ..Default::default()
        })
    }

    fn changeset(&self, _: &PullRequest) -> Result<Changeset, ForgeError> {
        let login = FileChange::text("login.rs", Some("fn login() {}\n"), Some("fn login() {\n    sso();\n}\n"));
        Ok(Changeset { base: "basesha".into(), files: vec![login] })
    }

    fn threads(&self, _: &PullRequest) -> Result<Vec<RemoteThread>, ForgeError> {
        Ok(vec![RemoteThread {
            id: "RT_1".into(),
            anchor: RemoteAnchor::Lines { path: "login.rs".into(), side: Side::New, start: 2, end: 2 },
            outdated: false,
            resolved_by: None,
            diff_hunk: String::new(),
            comments: vec![RemoteComment { author: "lee".into(), body: "Which SSO provider?".into() }],
        }])
    }

    fn viewer(&self) -> Result<String, ForgeError> {
        Ok("me".into())
    }

    fn verdicts(&self, _: &PullRequest, _: bool) -> Result<Vec<Verdict>, ForgeError> {
        Ok(Verdict::ALL.to_vec())
    }

    fn publish(&self, _: &PullRequest, review: &Review) -> Result<String, ForgeError> {
        self.published.lock().unwrap().push(review.clone());
        Ok(format!("{URL}#pullrequestreview-1"))
    }
}

fn pull_request() -> (Service, SessionId, Arc<Mutex<Vec<Review>>>) {
    let forge = FakeForge::default();
    let published = forge.published.clone();
    let svc = Service::new().with_forges(vec![Box::new(forge)]);
    let id = svc.open_pull_request(URL).unwrap();
    (svc, id, published)
}

/// Local work on branch `agent`, changing `lib.rs`.
fn agent_branch() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("lib.rs", "fn a() {}\nfn b() {}\n").commit_all("base");
    repo.branch("agent").write("lib.rs", "fn a() {}\nfn b2() {}\n");
    repo
}

/// Opens a Thread on line 2 of `lib.rs`, the new side.
fn on_b(svc: &Service, id: &SessionId, body: impl Into<NewComment>) -> ThreadId {
    svc.comment(id, Anchor::lines(Side::New, "lib.rs", 2..=2), body).unwrap()
}

/// A Session of `repo` whose first Round, T1, the agent has received, in the human's Turn
/// again with no `review` call waiting.
fn after_round_1(repo: &TestRepo, svc: &Service) -> SessionId {
    let first = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = first.session().clone();
    on_b(svc, &id, "Why rename?");
    svc.hand_over_round(&id).unwrap();
    first.wait_timeout(Duration::from_secs(1)).expect("the agent receives Round 1");
    drop(svc.review(ReviewRequest::new(repo.path())).unwrap());
    id
}

fn comment_at(id: &SessionId, thread: &str, comment: usize) -> CommentRef {
    CommentRef::new(&ThreadRef::new(id, &ThreadId::new(thread)), comment)
}

fn bodies(svc: &Service, id: &SessionId, thread: &str) -> Vec<String> {
    let session = svc.session(id).unwrap();
    let thread = session.threads.iter().find(|t| t.id.as_str() == thread).expect("the Thread is there");
    thread.comments.iter().map(|c| c.body.clone()).collect()
}

#[test]
fn editing_the_draft_that_opens_a_thread_changes_its_body_and_keeps_the_thread() {
    let repo = agent_branch();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = pending.session().clone();
    let t1 = on_b(&svc, &id, "Why rename?");
    let before = svc.session(&id).unwrap().threads[0].clone();

    svc.edit_own_draft(&comment_at(&id, "T1", 0), "Why rename `b`?").unwrap();

    let after = svc.session(&id).unwrap().threads[0].clone();
    assert_eq!((after.id.clone(), &after.anchor, &after.excerpt), (t1, &before.anchor, &before.excerpt));
    assert_eq!(after.comments.len(), 1);
    assert_eq!(after.comments[0].body, "Why rename `b`?");
    assert!(after.comments[0].is_draft() && after.comments[0].author == Party::Human);
}

#[test]
fn editing_a_draft_reply_changes_only_that_comment() {
    let repo = agent_branch();
    let svc = Service::new();
    let id = after_round_1(&repo, &svc);
    svc.reply(repo.path(), &ThreadId::new("T1"), "Clearer.").unwrap();
    let t1 = ThreadRef::new(&id, &ThreadId::new("T1"));
    svc.reply_in_thread(&t1, "First.").unwrap();
    svc.reply_in_thread(&t1, "Second.").unwrap();

    svc.edit_own_draft(&CommentRef::new(&t1, 3), "Second, edited.").unwrap();
    svc.edit_own_draft(&CommentRef::new(&t1, 2), "First, edited.").unwrap();

    assert_eq!(bodies(&svc, &id, "T1"), ["Why rename?", "Clearer.", "First, edited.", "Second, edited."]);
}

#[test]
fn an_edit_can_add_change_or_drop_a_suggestion_on_new_side_lines() {
    let repo = agent_branch();
    let svc = Service::new();
    let id = svc.review(ReviewRequest::new(repo.path())).unwrap().session().clone();
    on_b(&svc, &id, "Why rename?");
    let at = comment_at(&id, "T1", 0);
    let suggestion = || svc.session(&id).unwrap().threads[0].comments[0].suggestion.clone();

    svc.edit_own_draft(&at, NewComment::new("Keep it.").suggest("fn b() {}\n")).unwrap();
    assert_eq!(suggestion().as_deref(), Some("fn b() {}\n"));
    svc.edit_own_draft(&at, NewComment::new("Keep it.").suggest("fn b3() {}\n")).unwrap();
    assert_eq!(suggestion().as_deref(), Some("fn b3() {}\n"));
    svc.edit_own_draft(&at, "Keep it.").unwrap();

    assert_eq!(suggestion(), None);
}

#[test]
fn an_edit_with_a_suggestion_off_new_side_lines_or_without_text_is_refused_and_changes_nothing() {
    let repo = agent_branch();
    let svc = Service::new();
    let id = after_round_1(&repo, &svc);
    let t1 = ThreadRef::new(&id, &ThreadId::new("T1"));
    svc.reply(repo.path(), &ThreadId::new("T1"), "Clearer.").unwrap();
    svc.reply_in_thread(&t1, "A reply.").unwrap();
    let file = svc.comment(&id, Anchor::file("lib.rs"), "A file.").unwrap();
    let old = svc.comment(&id, Anchor::lines(Side::Old, "lib.rs", 2..=2), "An old line.").unwrap();
    let suggest = |c: &str| NewComment::new(c).suggest("x\n");

    let on_reply = svc.edit_own_draft(&CommentRef::new(&t1, 2), suggest("Reply."));
    let on_file = svc.edit_own_draft(&CommentRef::new(&ThreadRef::new(&id, &file), 0), suggest("File."));
    let on_old = svc.edit_own_draft(&CommentRef::new(&ThreadRef::new(&id, &old), 0), suggest("Old."));
    let empty = svc.edit_own_draft(&CommentRef::new(&t1, 2), "  \n");

    for refused in [on_reply, on_file, on_old] {
        assert!(matches!(refused, Err(Error::InvalidSuggestion)), "{refused:?}");
    }
    assert!(matches!(empty, Err(Error::EmptyComment)), "{empty:?}");
    assert_eq!(bodies(&svc, &id, "T1")[2], "A reply.");
    assert_eq!(bodies(&svc, &id, file.as_str()), ["A file."]);
    assert_eq!(bodies(&svc, &id, old.as_str()), ["An old line."]);
}

#[test]
fn discarding_a_draft_reply_leaves_the_thread_its_other_comments_and_its_resolved_state() {
    let repo = agent_branch();
    let svc = Service::new();
    let id = after_round_1(&repo, &svc);
    let t1 = ThreadRef::new(&id, &ThreadId::new("T1"));
    svc.reply(repo.path(), &ThreadId::new("T1"), "Clearer.").unwrap();
    svc.reply_in_thread(&t1, "First.").unwrap();
    svc.reply_in_thread(&t1, "Second.").unwrap();
    svc.resolve_thread(&t1).unwrap();

    svc.discard_own_draft(&CommentRef::new(&t1, 2)).unwrap();

    assert_eq!(bodies(&svc, &id, "T1"), ["Why rename?", "Clearer.", "Second."]);
    assert_eq!(svc.session(&id).unwrap().threads[0].resolved, Some(Party::Human));
}

#[test]
fn discarding_the_draft_that_opens_a_thread_removes_the_thread() {
    let repo = agent_branch();
    let svc = Service::new();
    let id = svc.review(ReviewRequest::new(repo.path())).unwrap().session().clone();
    on_b(&svc, &id, "Why rename?");
    let kept = svc.comment(&id, Anchor::Review, "Fine.").unwrap();

    svc.discard_own_draft(&comment_at(&id, "T1", 0)).unwrap();

    let threads: Vec<_> = svc.session(&id).unwrap().threads.iter().map(|t| t.id.clone()).collect();
    assert_eq!(threads, [kept]);
}

#[test]
fn the_next_round_carries_the_edited_text_and_not_the_discarded_drafts() {
    let repo = agent_branch();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = pending.session().clone();
    on_b(&svc, &id, "Why rename?");
    svc.comment(&id, Anchor::file("lib.rs"), "Split it?").unwrap();
    svc.comment(&id, Anchor::Review, "Nice.").unwrap();
    assert_eq!(svc.session(&id).unwrap().to_hand_over(), 3);

    svc.edit_own_draft(&comment_at(&id, "T1", 0), "Why rename `b`?").unwrap();
    svc.discard_own_draft(&comment_at(&id, "T2", 0)).unwrap();
    svc.discard_own_draft(&comment_at(&id, "T3", 0)).unwrap();

    assert_eq!(svc.session(&id).unwrap().to_hand_over(), 1);
    svc.hand_over_round(&id).unwrap();
    let Some(Outcome::Round(round)) = pending.wait_timeout(Duration::from_secs(1)) else { panic!("no Round") };
    let carried: Vec<_> = round.threads.iter().map(|t| (t.id.as_str(), t.comments[0].body.as_str())).collect();
    assert_eq!(carried, [("T1", "Why rename `b`?")]);
}

#[test]
fn the_publish_plan_holds_the_edited_text_and_not_the_discarded_drafts() {
    let (svc, id, published) = pull_request();
    let new = svc.comment(&id, Anchor::lines(Side::New, "login.rs", 2..=2), "Name it.").unwrap();
    let dropped = svc.comment(&id, Anchor::Review, "Meh.").unwrap();
    let remote = ThreadRef::new(&id, &svc.session(&id).unwrap().threads[0].id);
    svc.reply_in_thread(&remote, "Okta, I think.").unwrap();
    svc.reply_in_thread(&remote, "Never mind.").unwrap();

    svc.edit_own_draft(&comment_at(&id, new.as_str(), 0), "Name the provider.").unwrap();
    svc.edit_own_draft(&CommentRef::new(&remote, 1), "Okta.").unwrap();
    svc.discard_own_draft(&comment_at(&id, dropped.as_str(), 0)).unwrap();
    svc.discard_own_draft(&CommentRef::new(&remote, 2)).unwrap();

    let plan = svc.publish_plan(&id).unwrap();
    let texts = |threads: &[akshually_core::Thread]| {
        threads.iter().flat_map(|t| t.comments.iter().map(|c| c.body.clone())).collect::<Vec<_>>()
    };
    assert_eq!(texts(&plan.kept), ["Okta.", "Name the provider."]);
    assert!(plan.discarded.is_empty(), "{:?}", plan.discarded);
    svc.publish(&id, Verdict::Feedback, "").unwrap();
    let review = published.lock().unwrap()[0].clone();
    let sent: Vec<_> = review.comments.iter().map(|c| c.body.clone()).collect();
    assert_eq!(sent, ["Name the provider."]);
    assert_eq!(review.replies.len(), 1);
}

#[test]
fn a_co_reviewers_draft_the_human_kept_is_theirs_to_edit_and_discard_one_not_yet_kept_is_not() {
    let repo = agent_branch();
    let svc = Service::new();
    let kept = svc.draft(repo.path(), Anchor::Review, "Small.").unwrap();
    let unkept = svc.draft(repo.path(), Anchor::file("lib.rs"), "Split it?").unwrap();
    let id = svc.sessions()[0].id.clone();
    svc.keep_draft(&ThreadRef::new(&id, &kept)).unwrap();

    let refused = svc.edit_own_draft(&comment_at(&id, unkept.as_str(), 0), "Tiny.");
    let refused_discard = svc.discard_own_draft(&comment_at(&id, unkept.as_str(), 0));
    svc.edit_own_draft(&comment_at(&id, kept.as_str(), 0), "Tiny.").unwrap();
    svc.discard_own_draft(&comment_at(&id, kept.as_str(), 0)).unwrap();

    assert!(matches!(refused, Err(Error::NotYourDraft { .. })), "{refused:?}");
    assert!(matches!(refused_discard, Err(Error::NotYourDraft { .. })), "{refused_discard:?}");
    let session = svc.session(&id).unwrap();
    assert_eq!(session.threads.len(), 1);
    assert!(session.threads[0].comments[0].is_agent_draft(), "the unkept Draft changed");
}

#[test]
fn a_question_under_a_co_reviewers_draft_can_be_edited_and_discarded_while_it_is_a_draft() {
    let repo = agent_branch();
    let svc = Service::new();
    let draft = svc.draft(repo.path(), Anchor::Review, "Small.").unwrap();
    let id = svc.sessions()[0].id.clone();
    let at = ThreadRef::new(&id, &draft);
    svc.reply_in_thread(&at, "Why?").unwrap();
    svc.reply_in_thread(&at, "And?").unwrap();

    svc.edit_own_draft(&CommentRef::new(&at, 1), "Why small?").unwrap();
    svc.discard_own_draft(&CommentRef::new(&at, 2)).unwrap();

    assert_eq!(bodies(&svc, &id, draft.as_str()), ["Small.", "Why small?"]);
    assert!(svc.session(&id).unwrap().threads[0].comments[0].is_agent_draft(), "the Draft stays the agent's");
}

#[test]
fn after_taking_the_turn_back_the_comments_of_a_round_the_agent_never_received_can_be_edited_and_discarded() {
    let repo = agent_branch();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = pending.session().clone();
    // No `review` call is waiting, so the Round stays queued for the agent.
    drop(pending);
    on_b(&svc, &id, "Why rename?");
    svc.comment(&id, Anchor::Review, "Nice.").unwrap();
    svc.hand_over_round(&id).unwrap();
    let carried = svc.edit_own_draft(&comment_at(&id, "T1", 0), "Too late.");
    assert!(matches!(carried, Err(Error::NotYourDraft { .. })), "{carried:?}");
    svc.take_turn_back(&id).unwrap();

    svc.edit_own_draft(&comment_at(&id, "T1", 0), "Why rename `b`?").unwrap();
    svc.discard_own_draft(&comment_at(&id, "T2", 0)).unwrap();

    let session = svc.session(&id).unwrap();
    assert_eq!(bodies(&svc, &id, "T1"), ["Why rename `b`?"]);
    assert_eq!(session.threads.len(), 1);
    assert_eq!(session.to_hand_over(), 1);
}

#[test]
fn a_comment_a_round_carried_the_agent_wrote_or_the_forge_had_is_refused_and_nothing_changes() {
    let repo = agent_branch();
    let svc = Service::new();
    let id = after_round_1(&repo, &svc);
    svc.reply(repo.path(), &ThreadId::new("T1"), "Clearer.").unwrap();
    let before = svc.session(&id).unwrap().threads;
    let (on_forge, forge_id, _) = pull_request();
    let remote = ThreadRef::new(&forge_id, &on_forge.session(&forge_id).unwrap().threads[0].id);

    let results = [
        svc.edit_own_draft(&comment_at(&id, "T1", 0), "Edited."),
        svc.discard_own_draft(&comment_at(&id, "T1", 0)),
        svc.edit_own_draft(&comment_at(&id, "T1", 1), "Edited."),
        svc.discard_own_draft(&comment_at(&id, "T1", 1)),
        svc.edit_own_draft(&comment_at(&id, "T1", 7), "Edited."),
        on_forge.edit_own_draft(&CommentRef::new(&remote, 0), "Edited."),
        on_forge.discard_own_draft(&CommentRef::new(&remote, 0)),
    ];

    for result in results {
        assert!(matches!(result, Err(Error::NotYourDraft { .. })), "{result:?}");
    }
    assert_eq!(svc.session(&id).unwrap().threads, before);
    assert_eq!(on_forge.session(&forge_id).unwrap().threads[0].comments.len(), 1);
}

#[test]
fn a_reference_to_a_thread_that_is_gone_is_refused_and_does_not_reach_another_comment() {
    let repo = agent_branch();
    let svc = Service::new();
    let id = svc.review(ReviewRequest::new(repo.path())).unwrap().session().clone();
    on_b(&svc, &id, "Why rename?");
    svc.discard_own_draft(&comment_at(&id, "T1", 0)).unwrap();
    let other = svc.comment(&id, Anchor::Review, "Fine.").unwrap();

    let stale = svc.edit_own_draft(&comment_at(&id, "T1", 0), "Edited.");
    let unknown = svc.discard_own_draft(&comment_at(&id, "T9", 0));

    assert!(matches!(stale, Err(Error::UnknownThread(_))), "{stale:?}");
    assert!(matches!(unknown, Err(Error::UnknownThread(_))), "{unknown:?}");
    assert_eq!(bodies(&svc, &id, other.as_str()), ["Fine."]);
}

#[test]
fn a_position_that_no_longer_holds_a_draft_is_refused_rather_than_applied_to_another_comment() {
    let repo = agent_branch();
    let svc = Service::new();
    let id = after_round_1(&repo, &svc);
    svc.reply(repo.path(), &ThreadId::new("T1"), "Clearer.").unwrap();
    let t1 = ThreadRef::new(&id, &ThreadId::new("T1"));
    svc.reply_in_thread(&t1, "First.").unwrap();
    svc.discard_own_draft(&CommentRef::new(&t1, 2)).unwrap();

    let stale = svc.edit_own_draft(&CommentRef::new(&t1, 2), "Edited.");

    assert!(matches!(stale, Err(Error::NotYourDraft { .. })), "{stale:?}");
    assert_eq!(bodies(&svc, &id, "T1"), ["Why rename?", "Clearer."]);
}

#[test]
fn editing_or_discarding_in_an_ended_session_is_refused() {
    let repo = agent_branch();
    let svc = Service::new();
    let id = svc.review(ReviewRequest::new(repo.path())).unwrap().session().clone();
    on_b(&svc, &id, "Why rename?");
    svc.approve(&id).unwrap();

    let edited = svc.edit_own_draft(&comment_at(&id, "T1", 0), "Edited.");
    let discarded = svc.discard_own_draft(&comment_at(&id, "T1", 0));

    assert!(matches!(edited, Err(Error::Ended)), "{edited:?}");
    assert!(matches!(discarded, Err(Error::Ended)), "{discarded:?}");
}

#[test]
fn the_session_offers_edit_and_discard_on_exactly_the_humans_drafts_and_on_none_once_it_ended() {
    let repo = agent_branch();
    let svc = Service::new();
    let id = after_round_1(&repo, &svc);
    svc.reply(repo.path(), &ThreadId::new("T1"), "Clearer.").unwrap();
    let t1 = ThreadRef::new(&id, &ThreadId::new("T1"));
    svc.reply_in_thread(&t1, "First.").unwrap();
    svc.comment(&id, Anchor::Review, "New.").unwrap();
    let own = |svc: &Service, thread: &str| {
        let actions = svc.session(&id).unwrap().actions();
        actions.thread(&ThreadId::new(thread)).unwrap().own_drafts.clone()
    };

    assert_eq!((own(&svc, "T1"), own(&svc, "T2")), (vec![2], vec![0]));

    svc.approve(&id).unwrap();
    assert_eq!((own(&svc, "T1"), own(&svc, "T2")), (vec![], vec![]));
}

#[test]
fn an_edited_and_a_discarded_draft_are_still_so_after_the_session_is_reloaded_from_disk() {
    let repo = agent_branch();
    let dirs = TestDirs::new();
    let id = {
        let svc = Service::open(dirs.state()).unwrap();
        let id = svc.review(ReviewRequest::new(repo.path())).unwrap().session().clone();
        on_b(&svc, &id, "Why rename?");
        svc.comment(&id, Anchor::Review, "Nice.").unwrap();
        svc.edit_own_draft(&comment_at(&id, "T1", 0), "Why rename `b`?").unwrap();
        svc.discard_own_draft(&comment_at(&id, "T2", 0)).unwrap();
        id
    };

    let restored = Service::open(dirs.state()).unwrap();

    let session = restored.session(&id).unwrap();
    assert_eq!(session.threads.len(), 1);
    assert_eq!(session.threads[0].comments[0].body, "Why rename `b`?");
    assert!(session.threads[0].comments[0].is_draft());
}
