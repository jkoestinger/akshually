//! Seam A: an agent as co-Reviewer of a Pull Request. It adds Drafts, the human keeps, edits
//! or discards them, and the agent's blocking `review` call gets what the human Published.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use akshually_core::forge::{ForgeAdapter, ForgeError, PullRequest, PullRequestRef, RemoteThread, Review};
use akshually_core::{
    Anchor, Changeset, Error, FileChange, NewComment, Outcome, Party, ReviewRequest, Role, Service, Side, Turn, Verdict,
};
use akshually_testkit::TestRepo;

const URL: &str = "https://forge.test/acme/web/pull/7";

/// A Forge with one Pull Request, `acme/web#7`, that records each review published.
#[derive(Default)]
struct FakeForge {
    published: Arc<Mutex<Vec<Review>>>,
}

fn pr_ref() -> PullRequestRef {
    PullRequestRef { host: "forge.test".into(), repo: "acme/web".into(), number: 7 }
}

impl ForgeAdapter for FakeForge {
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
        })
    }

    fn changeset(&self, _: &PullRequest) -> Result<Changeset, ForgeError> {
        Ok(Changeset {
            base: "basesha".into(),
            files: vec![
                FileChange::text("login.rs", Some("fn login() {}\n"), Some("fn login() {\n    sso();\n}\n")),
                FileChange::text("README.md", Some("# Web\n"), Some("# Web\n\nSSO.\n")),
            ],
        })
    }

    fn threads(&self, _: &PullRequest) -> Result<Vec<RemoteThread>, ForgeError> {
        Ok(Vec::new())
    }

    fn verdicts(&self, _: &PullRequest) -> Result<Vec<Verdict>, ForgeError> {
        Ok(Verdict::ALL.to_vec())
    }

    fn publish(&self, _: &PullRequest, review: &Review) -> Result<String, ForgeError> {
        self.published.lock().unwrap().push(review.clone());
        Ok(format!("{URL}#pullrequestreview-1"))
    }
}

fn service() -> (Service, Arc<Mutex<Vec<Review>>>) {
    let forge = FakeForge::default();
    let published = forge.published.clone();
    (Service::new().with_forges(vec![Box::new(forge)]), published)
}

#[test]
fn an_agent_draft_opens_the_pull_request_with_the_agent_as_co_reviewer() {
    let (svc, _) = service();

    let thread = svc.draft(URL, Anchor::lines(Side::New, "login.rs", 2..=2), "Which provider?").unwrap();

    let sessions = svc.sessions();
    assert_eq!(sessions.len(), 1);
    let session = &sessions[0];
    assert_eq!(session.pull_request.as_ref().unwrap().url, URL);
    assert_eq!(session.agent, Some(Role::CoReviewer));
    assert_eq!(session.turn, Turn::Agent, "the agent is still reviewing");
    let draft = &session.threads.iter().find(|t| t.id == thread).unwrap().comments[0];
    assert_eq!((draft.author.clone(), draft.body.as_str()), (Party::Agent, "Which provider?"));
    assert!(draft.is_agent_draft(), "not kept yet");
}

#[test]
fn drafts_take_every_anchor_kind_and_suggestions_into_the_same_session() {
    let (svc, _) = service();

    let lines = svc
        .draft(
            URL,
            Anchor::lines(Side::New, "login.rs", 2..=2),
            NewComment::from_markdown("Name it.\n\n```suggestion\n    okta();\n```\n"),
        )
        .unwrap();
    let file = svc.draft(URL, Anchor::file("README.md"), "Link the docs.").unwrap();
    let review = svc.draft(URL, Anchor::Review, "Small and clear.").unwrap();

    let sessions = svc.sessions();
    assert_eq!(sessions.len(), 1, "one Session per Pull Request");
    let anchors: Vec<_> = sessions[0].threads.iter().map(|t| (t.id.clone(), t.anchor.clone())).collect();
    assert_eq!(
        anchors,
        vec![
            (lines, Anchor::lines(Side::New, "login.rs", 2..=2)),
            (file, Anchor::file("README.md")),
            (review, Anchor::Review),
        ]
    );
    let first = &sessions[0].threads[0].comments[0];
    assert_eq!((first.body.as_str(), first.suggestion.as_deref()), ("Name it.", Some("    okta();\n")));
}

#[test]
fn a_draft_that_does_not_fit_the_changeset_is_refused() {
    let (svc, _) = service();

    let outside = svc.draft(URL, Anchor::lines(Side::New, "login.rs", 9..=9), "Hm.");
    let suggestion = svc.draft(URL, Anchor::file("README.md"), NewComment::new("Hm.").suggest("x"));

    assert!(matches!(outside, Err(Error::InvalidAnchor(_))), "{outside:?}");
    assert!(matches!(suggestion, Err(Error::InvalidSuggestion)), "{suggestion:?}");
}

#[test]
fn an_agent_draft_does_not_take_the_view_from_the_human() {
    let (svc, _) = service();
    let mine = svc.open_pull_request(URL).unwrap();

    svc.draft(URL, Anchor::Review, "Small and clear.").unwrap();

    assert_eq!(svc.displayed(), Some(mine.clone()));
    let session = svc.session(&mine).unwrap();
    assert_eq!(session.agent, Some(Role::CoReviewer));
    assert_eq!(session.turn, Turn::Human, "the human was reviewing already");
}

#[test]
fn a_kept_agent_draft_becomes_the_humans_and_only_kept_drafts_are_published() {
    let (svc, published) = service();
    let kept = svc.draft(URL, Anchor::lines(Side::New, "login.rs", 2..=2), "Name the provider.").unwrap();
    let edited = svc.draft(URL, Anchor::file("README.md"), "Docs?").unwrap();
    let discarded = svc.draft(URL, Anchor::Review, "Looks fine.").unwrap();
    let unvetted = svc.draft(URL, Anchor::file("login.rs"), "Needs a test.").unwrap();
    let id = svc.sessions()[0].id.clone();

    svc.keep_draft(&id, &kept).unwrap();
    svc.edit_draft(&id, &edited, "Link the SSO docs here.").unwrap();
    svc.discard_draft(&id, &discarded).unwrap();

    let session = svc.session(&id).unwrap();
    let thread = |t| session.threads.iter().find(|thread| &thread.id == t);
    let author = |t| thread(t).unwrap().comments[0].author.clone();
    assert_eq!(author(&kept), Party::Human, "indistinguishable from the human's");
    assert_eq!(author(&edited), Party::Human, "the human wrote what it says now");
    assert_eq!(thread(&edited).unwrap().comments[0].body, "Link the SSO docs here.");
    assert!(thread(&discarded).is_none());
    assert!(thread(&unvetted).unwrap().comments[0].is_agent_draft());
    let plan = svc.publish_plan(&id).unwrap();
    let ids = |threads: &[akshually_core::Thread]| threads.iter().map(|t| t.id.clone()).collect::<Vec<_>>();
    assert_eq!(ids(&plan.kept), vec![kept, edited]);
    assert_eq!(ids(&plan.discarded), vec![unvetted]);

    svc.publish(&id, Verdict::RequestChanges, "").unwrap();

    let bodies: Vec<_> = published.lock().unwrap()[0].comments.iter().map(|c| c.body.clone()).collect();
    assert_eq!(bodies, vec!["Name the provider.", "Link the SSO docs here."]);
}

#[test]
fn the_co_reviewers_review_call_hands_its_drafts_to_the_human_and_returns_what_was_published() {
    let (svc, _) = service();
    let thread = svc.draft(URL, Anchor::file("README.md"), "Link the docs.").unwrap();
    let id = svc.sessions()[0].id.clone();
    svc.display(&id).unwrap();

    let pending = svc.review(ReviewRequest::pull_request(URL)).unwrap();

    let session = svc.session(&id).unwrap();
    assert_eq!((session.turn, session.agent_waiting), (Turn::Human, true));
    assert!(pending.wait_timeout(Duration::from_millis(50)).is_none(), "blocks until the human is done");
    svc.keep_draft(&id, &thread).unwrap();
    let url = svc.publish(&id, Verdict::Approve, "").unwrap();
    assert_eq!(pending.wait(), Some(Outcome::Published { verdict: Verdict::Approve, url }));
    assert!(svc.sessions().is_empty(), "the Session ends once the co-Reviewer has its outcome");
}

#[test]
fn a_co_review_the_human_publishes_before_the_agent_asks_is_kept_for_its_review_call() {
    let (svc, _) = service();
    svc.draft(URL, Anchor::Review, "Small and clear.").unwrap();
    let id = svc.sessions()[0].id.clone();

    let url = svc.publish(&id, Verdict::Approve, "LGTM").unwrap();

    assert!(matches!(svc.close(&id), Err(Error::Ended)), "the co-Reviewer hasn't been told yet");
    let pending = svc.review(ReviewRequest::pull_request(URL)).unwrap();
    assert_eq!(pending.wait(), Some(Outcome::Published { verdict: Verdict::Approve, url }));
    assert!(svc.sessions().is_empty());
}

#[test]
fn closing_or_abandoning_a_co_review_tells_the_co_reviewer_it_was_abandoned() {
    for end in [Service::close, Service::abandon] {
        let (svc, published) = service();
        let pending = svc.review(ReviewRequest::pull_request(URL)).unwrap();
        let id = pending.session().clone();

        end(&svc, &id).unwrap();

        assert_eq!(pending.wait(), Some(Outcome::Abandoned));
        assert!(svc.sessions().is_empty());
        assert!(published.lock().unwrap().is_empty());
    }
}

#[test]
fn a_session_never_has_more_than_one_agent() {
    let (svc, _) = service();
    let pending = svc.review(ReviewRequest::pull_request(URL)).unwrap();

    let second = svc.review(ReviewRequest::pull_request(URL));

    assert!(matches!(second, Err(Error::Busy)), "{:?}", second.err());
    drop(pending);
    let id = svc.sessions()[0].id.clone();
    assert_eq!(svc.session(&id).unwrap().agent, Some(akshually_core::Role::CoReviewer));
}

#[test]
fn only_an_agent_draft_can_be_kept_or_discarded() {
    let (svc, _) = service();
    svc.draft(URL, Anchor::Review, "Looks fine.").unwrap();
    let id = svc.sessions()[0].id.clone();
    let mine = svc.comment(&id, Anchor::file("README.md"), "Docs?").unwrap();

    for result in [svc.keep_draft(&id, &mine), svc.discard_draft(&id, &mine), svc.edit_draft(&id, &mine, "x")] {
        assert!(matches!(result, Err(Error::NotAnAgentDraft(_))), "{result:?}");
    }
}

#[test]
fn questions_on_agent_drafts_reach_the_co_reviewer_as_a_round_and_the_drafts_stay_unkept() {
    let (svc, _) = service();
    let questioned = svc.draft(URL, Anchor::lines(Side::New, "login.rs", 2..=2), "Validate the state.").unwrap();
    let other = svc.draft(URL, Anchor::Review, "Small and clear.").unwrap();
    let pending = svc.review(ReviewRequest::pull_request(URL)).unwrap();
    let id = pending.session().clone();
    let mine = svc.comment(&id, Anchor::file("README.md"), "Docs?").unwrap();

    svc.reply_in_thread(&id, &questioned, "Which state? There are two.").unwrap();
    let number = svc.hand_over_round(&id).unwrap();

    let Some(Outcome::Round(round)) = pending.wait() else { panic!("expected a Round") };
    assert_eq!(round.number, number);
    let carried: Vec<_> = round.threads.iter().map(|t| (t.id.clone(), t.comments.len())).collect();
    assert_eq!(carried, vec![(questioned.clone(), 1)], "only the question, not the human's own Drafts");
    assert_eq!(round.threads[0].comments[0].body, "Which state? There are two.");
    let session = svc.session(&id).unwrap();
    assert_eq!(session.turn, Turn::Agent);
    let thread = |t| session.threads.iter().find(|thread| &thread.id == t).unwrap().clone();
    assert!(thread(&questioned).comments[0].is_agent_draft(), "still for the human to keep or discard");
    assert!(thread(&other).comments[0].is_agent_draft());
    assert!(thread(&mine).comments[0].is_draft(), "the human's Draft waits for Publish");
}

#[test]
fn the_co_reviewer_answers_questions_in_their_threads_and_its_next_review_call_is_the_humans_turn() {
    let (svc, published) = service();
    let questioned = svc.draft(URL, Anchor::lines(Side::New, "login.rs", 2..=2), "Validate the state.").unwrap();
    let unasked = svc.draft(URL, Anchor::Review, "Small and clear.").unwrap();
    let pending = svc.review(ReviewRequest::pull_request(URL)).unwrap();
    let id = pending.session().clone();
    svc.reply_in_thread(&id, &questioned, "Which state?").unwrap();
    svc.hand_over_round(&id).unwrap();
    pending.wait().unwrap();

    svc.reply(URL, &questioned, "The OAuth `state` parameter.").unwrap();
    let refused = svc.reply(URL, &unasked, "Never asked.");
    let pending = svc.review(ReviewRequest::pull_request(URL)).unwrap();

    assert!(matches!(refused, Err(Error::UnknownThread(_))), "{refused:?}");
    let session = svc.session(&id).unwrap();
    assert_eq!(session.turn, Turn::Human);
    let thread = session.threads.iter().find(|t| t.id == questioned).unwrap();
    let conversation: Vec<_> = thread.comments.iter().map(|c| (c.author.clone(), c.body.as_str())).collect();
    assert_eq!(
        conversation,
        vec![
            (Party::Agent, "Validate the state."),
            (Party::Human, "Which state?"),
            (Party::Agent, "The OAuth `state` parameter."),
        ]
    );
    svc.keep_draft(&id, &questioned).unwrap();
    let url = svc.publish(&id, Verdict::RequestChanges, "").unwrap();
    assert_eq!(pending.wait(), Some(Outcome::Published { verdict: Verdict::RequestChanges, url }));
    let bodies: Vec<_> = published.lock().unwrap()[0].comments.iter().map(|c| c.body.clone()).collect();
    assert_eq!(bodies, vec!["Validate the state."], "the questions and answers stay between the two");
}

/// Local work on branch `feature`: `lib.rs` gained `b`.
fn local_work() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("lib.rs", "fn a() {}\n").commit_all("base");
    repo.branch("feature").write("lib.rs", "fn a() {}\nfn b() {}\n").commit_all("add b");
    repo
}

#[test]
fn a_draft_on_local_work_opens_its_session_with_the_agent_as_co_reviewer() {
    let repo = local_work();
    let svc = Service::new();

    let thread = svc.draft(repo.path(), Anchor::lines(Side::New, "lib.rs", 2..=2), "Name it.").unwrap();
    svc.draft(repo.path(), Anchor::Review, "Small.").unwrap();

    let sessions = svc.sessions();
    assert_eq!(sessions.len(), 1, "one Session per repository and branch");
    let session = &sessions[0];
    assert_eq!((session.repo.as_path(), session.branch.as_str()), (repo.path(), "feature"));
    assert_eq!((session.agent, session.turn), (Some(Role::CoReviewer), Turn::Agent));
    assert_eq!(session.changeset.files[0].path, "lib.rs");
    assert_eq!(session.threads[0].id, thread);
    assert!(session.threads.iter().all(|t| t.comments[0].is_agent_draft()));
    assert_eq!(svc.displayed(), None, "the human isn't asked until the agent calls `review`");
}

#[test]
fn a_local_co_review_goes_through_question_rounds_and_closing_it_returns_closed() {
    let repo = local_work();
    let svc = Service::new();
    let thread = svc.draft(repo.path(), Anchor::lines(Side::New, "lib.rs", 2..=2), "Name it.").unwrap();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = pending.session().clone();
    assert_eq!(svc.session(&id).unwrap().turn, Turn::Human);

    svc.reply_in_thread(&id, &thread, "Name it what?").unwrap();
    svc.hand_over_round(&id).unwrap();
    let Some(Outcome::Round(round)) = pending.wait() else { panic!("expected a Round") };
    assert_eq!(round.threads[0].comments[0].body, "Name it what?");
    svc.reply(repo.path(), &thread, "`second`.").unwrap();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();

    let session = svc.session(&id).unwrap();
    assert_eq!((session.agent, session.turn), (Some(Role::CoReviewer), Turn::Human));
    assert_eq!(session.threads[0].comments.last().unwrap().body, "`second`.");
    assert!(matches!(svc.approve(&id), Err(Error::InvalidRequest(_))), "a co-review has no Verdict");
    svc.close(&id).unwrap();
    assert_eq!(pending.wait(), Some(Outcome::Closed));
    assert!(svc.sessions().is_empty());
}

#[test]
fn abandoning_a_local_co_review_closes_it_normally() {
    let repo = local_work();
    let svc = Service::new();
    svc.draft(repo.path(), Anchor::Review, "Small.").unwrap();
    let id = svc.sessions()[0].id.clone();

    svc.abandon(&id).unwrap();

    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    assert_eq!(pending.wait(), Some(Outcome::Closed), "kept for the agent's next call");
    assert!(svc.sessions().is_empty());
}

#[test]
fn local_work_has_at_most_one_agent_in_one_role() {
    let repo = local_work();
    let svc = Service::new();
    let author = svc.review(ReviewRequest::new(repo.path())).unwrap();

    let draft = svc.draft(repo.path(), Anchor::Review, "Small.");

    assert!(matches!(draft, Err(Error::OtherAgent(Role::Author))), "{draft:?}");
    assert!(matches!(svc.close(author.session()), Err(Error::InvalidRequest(_))), "an Author is abandoned");
}
