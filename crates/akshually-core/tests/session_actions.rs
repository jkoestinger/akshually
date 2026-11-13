//! Seam A: what the human can do in a Session (`SessionView::actions`), across Source, agent
//! role, Turn and whether it has ended, and the service refusing every action it doesn't offer;
//! then the Changeset views a Session offers, and what its tab marks it with.

use std::time::Duration;

use akshually_core::forge::{
    ForgeAdapter, ForgeError, PullRequest, PullRequestRef, PullRequestUpdate, RemoteThread, Review,
};
use akshually_core::{
    Anchor, Changeset, ChangesetView, Comment, EndAction, Ending, Error, FileChange, HandOver, Party, ReviewRequest,
    Role, Service, SessionId, SessionMark, SessionView, Side, Source, Thread, ThreadActions, ThreadId, ThreadRef, Turn,
    Verdict, VerdictAction,
};
use akshually_testkit::TestRepo;

const PR_URL: &str = "https://forge.test/acme/web/pull/7";

fn pr() -> PullRequest {
    PullRequest {
        id: PullRequestRef { host: "forge.test".into(), repo: "acme/web".into(), number: 7 },
        url: PR_URL.into(),
        title: "Add SSO login".into(),
        author: "maria".into(),
        head_branch: "sso".into(),
        head_sha: "headsha".into(),
        base_sha: "basesha".into(),
    }
}

/// A Forge with one Pull Request, `acme/web#7`, changing `login.rs`.
struct FakeForge;

impl ForgeAdapter for FakeForge {
    fn recognise(&self, url: &str) -> Option<PullRequestRef> {
        url.starts_with(PR_URL).then(|| pr().id)
    }

    fn pull_request(&self, _: &PullRequestRef) -> Result<PullRequest, ForgeError> {
        Ok(pr())
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

fn local(agent: Role, turn: Turn, ended: Option<Ending>) -> SessionView {
    SessionView {
        source: Source::Local { repo: "/work/web".into(), branch: "sso".into() },
        agent: Some(agent),
        round: 1,
        turn,
        ended,
        ..SessionView::default()
    }
}

fn pull_request(agent: Option<Role>, turn: Turn, ended: Option<Ending>) -> SessionView {
    SessionView { source: Source::PullRequest(pr()), agent, round: 1, turn, ended, ..SessionView::default() }
}

fn published() -> Option<Ending> {
    Some(Ending::Published { verdict: Verdict::Feedback, url: "https://forge.test/r/1".into() })
}

// --- The Verdict and how the Session ends ---

/// Every Session the table covers: Source × agent role × Turn × ended, named.
fn every_session() -> [(&'static str, SessionView); 11] {
    use Role::{Author, CoReviewer};
    use Turn::{Agent, Human};
    [
        ("local work, the human's Turn", local(Author, Human, None)),
        ("local work, the agent's Turn", local(Author, Agent, None)),
        ("local work approved", local(Author, Human, Some(Ending::Approved))),
        ("local work abandoned", local(Author, Agent, Some(Ending::Abandoned))),
        ("a local co-review, the human's Turn", local(CoReviewer, Human, None)),
        ("a local co-review, the agent's Turn", local(CoReviewer, Agent, None)),
        ("a local co-review closed", local(CoReviewer, Human, Some(Ending::Closed))),
        ("a Pull Request alone", pull_request(None, Human, None)),
        ("a Pull Request, the co-Reviewer's Turn", pull_request(Some(CoReviewer), Agent, None)),
        ("a Pull Request, the human's Turn", pull_request(Some(CoReviewer), Human, None)),
        ("a Pull Request Published", pull_request(None, Human, published())),
    ]
}

#[test]
fn the_verdict_is_approve_in_the_humans_turn_and_take_turn_back_in_the_agents_on_local_work_only() {
    use VerdictAction::{Approve, TakeTurnBack};
    let expected =
        [Some(Approve), Some(TakeTurnBack), None, None, None, Some(TakeTurnBack), None, None, None, None, None];

    let verdicts = every_session().map(|(what, session)| (what, session.actions().verdict));

    assert_eq!(verdicts, std::array::from_fn(|i| (every_session()[i].0, expected[i])));
}

#[test]
fn a_session_ends_by_abandoning_it_unless_it_has_its_outcome_or_is_a_local_co_review() {
    use EndAction::{Abandon, Close};
    let expected = [Abandon, Abandon, Close, Close, Close, Close, Close, Abandon, Abandon, Abandon, Close];

    let endings = every_session().map(|(what, session)| (what, session.actions().end));

    assert_eq!(endings, std::array::from_fn(|i| (every_session()[i].0, expected[i])));
}

// --- Handing a Round over ---

/// A Thread `id` on the Review as a whole with `comments`.
fn thread(id: &str, comments: Vec<Comment>) -> Thread {
    Thread {
        id: ThreadId::new(id),
        anchor: Anchor::Review,
        excerpt: String::new(),
        comments,
        resolved: None,
        outdated: false,
        reopened: false,
        forge: None,
    }
}

/// A Comment by `author`, carried by `round` (`None`: a Draft).
fn comment(author: Party, round: Option<u32>) -> Comment {
    Comment { author, body: "Why?".into(), round, suggestion: None }
}

fn with_threads(session: SessionView, threads: Vec<Thread>) -> SessionView {
    SessionView { threads, ..session }
}

#[test]
fn a_round_with_drafts_can_be_handed_over_in_the_humans_turn() {
    let drafts = vec![thread("T1", vec![comment(Party::Human, None)]), thread("T2", vec![comment(Party::Human, None)])];
    let session = with_threads(local(Role::Author, Turn::Human, None), drafts);

    let hand_over = session.actions().hand_over;

    assert_eq!(hand_over, Some(HandOver { items: 2, offered: true }));
}

#[test]
fn a_round_with_nothing_to_carry_can_t_be_handed_over() {
    let sent = vec![thread("T1", vec![comment(Party::Human, Some(1))])];
    let session = with_threads(local(Role::Author, Turn::Human, None), sent);

    let hand_over = session.actions().hand_over;

    assert_eq!(hand_over, Some(HandOver { items: 0, offered: false }));
}

#[test]
fn a_round_can_t_be_handed_over_in_the_agents_turn() {
    let drafts = vec![thread("T1", vec![comment(Party::Human, None)])];
    let session = with_threads(local(Role::Author, Turn::Agent, None), drafts);

    let hand_over = session.actions().hand_over;

    assert_eq!(hand_over, Some(HandOver { items: 1, offered: false }));
}

#[test]
fn a_round_can_t_be_handed_over_once_the_session_ended() {
    let drafts = vec![thread("T1", vec![comment(Party::Human, None)])];
    let session = with_threads(local(Role::Author, Turn::Human, Some(Ending::Approved)), drafts);

    let hand_over = session.actions().hand_over;

    assert_eq!(hand_over, Some(HandOver { items: 1, offered: false }));
}

#[test]
fn a_pull_request_without_an_agent_has_no_round_to_hand_over() {
    let session = pull_request(None, Turn::Human, None);

    let hand_over = session.actions().hand_over;

    assert_eq!(hand_over, None);
}

// --- Threads ---

fn thread_actions(session: SessionView, thread: Thread) -> ThreadActions {
    let id = thread.id.clone();
    with_threads(session, vec![thread]).actions().thread(&id).cloned().expect("the Session has the Thread")
}

fn offered(id: &str, reply: bool, resolve: bool, reopen: bool, agent_draft: bool) -> ThreadActions {
    ThreadActions { thread: ThreadId::new(id), reply, resolve, reopen, agent_draft }
}

#[test]
fn a_thread_still_all_drafts_offers_nothing_yet() {
    let draft = thread("T1", vec![comment(Party::Human, None)]);

    let actions = thread_actions(local(Role::Author, Turn::Human, None), draft);

    assert_eq!(actions, offered("T1", false, false, false, false));
}

#[test]
fn a_thread_the_agent_received_can_be_replied_to_and_resolved_in_either_turn() {
    let sent = || thread("T1", vec![comment(Party::Human, Some(1)), comment(Party::Agent, Some(2))]);

    let yours = thread_actions(local(Role::Author, Turn::Human, None), sent());
    let agents = thread_actions(local(Role::Author, Turn::Agent, None), sent());

    assert_eq!(yours, offered("T1", true, true, false, false));
    assert_eq!(agents, offered("T1", true, true, false, false), "the human drafts on during the agent's Turn");
}

#[test]
fn a_resolved_thread_can_be_reopened_rather_than_resolved() {
    let resolved = Thread { resolved: Some(Party::Agent), ..thread("T1", vec![comment(Party::Human, Some(1))]) };

    let actions = thread_actions(local(Role::Author, Turn::Human, None), resolved);

    assert_eq!(actions, offered("T1", true, false, true, false));
}

#[test]
fn a_thread_already_on_the_forge_can_be_replied_to_and_resolved() {
    let on_forge = thread("T1", vec![comment(Party::Other("lee".into()), None)]);

    let actions = thread_actions(pull_request(None, Turn::Human, None), on_forge);

    assert_eq!(actions, offered("T1", true, true, false, false));
}

#[test]
fn an_agent_draft_can_be_kept_discarded_or_questioned() {
    let agent_draft = thread("T1", vec![comment(Party::Agent, None)]);

    let actions = thread_actions(pull_request(Some(Role::CoReviewer), Turn::Agent, None), agent_draft);

    assert_eq!(actions, offered("T1", true, false, false, true));
}

#[test]
fn an_ended_session_offers_nothing_in_its_threads() {
    let resolved = Thread { resolved: Some(Party::Human), ..thread("T1", vec![comment(Party::Human, Some(1))]) };
    let agent_draft = thread("T2", vec![comment(Party::Agent, None)]);
    let ended = pull_request(Some(Role::CoReviewer), Turn::Human, published());
    let session = with_threads(ended, vec![resolved, agent_draft]);

    let actions = session.actions();

    let nothing = |id| offered(id, false, false, false, false);
    assert_eq!(actions.threads, vec![nothing("T1"), nothing("T2")]);
}

// --- The service refuses what isn't offered ---

/// Local work on branch `agent`, changing `lib.rs`.
fn agent_branch() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("lib.rs", "fn a() {}\nfn b() {}\n").commit_all("base");
    repo.branch("agent").write("lib.rs", "fn a() {}\nfn b2() {}\n");
    repo
}

/// A Session of `repo` whose first Round, T1, the agent has received, in the human's Turn
/// again with no `review` call waiting.
fn after_round_1(repo: &TestRepo, svc: &Service) -> SessionId {
    let first = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = first.session().clone();
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 2..=2), "Why rename?").unwrap();
    svc.hand_over_round(&id).unwrap();
    first.wait_timeout(Duration::from_secs(1)).expect("the agent receives Round 1");
    drop(svc.review(ReviewRequest::new(repo.path())).unwrap());
    id
}

#[test]
fn replying_in_a_thread_the_agent_has_not_received_is_refused() {
    let repo = agent_branch();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let draft = svc.comment(pending.session(), Anchor::Review, "Small.").unwrap();

    let reply = svc.reply_in_thread(&ThreadRef::new(pending.session(), &draft), "And clear.");

    assert!(matches!(reply, Err(Error::NotSent)), "{reply:?}");
    assert_eq!(svc.session(pending.session()).unwrap().threads[0].comments.len(), 1, "the reply went in");
}

#[test]
fn resolving_a_thread_the_agent_has_not_received_is_refused() {
    let repo = agent_branch();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let draft = svc.comment(pending.session(), Anchor::Review, "Small.").unwrap();

    let resolved = svc.resolve_thread(&ThreadRef::new(pending.session(), &draft));

    assert!(matches!(resolved, Err(Error::NotSent)), "{resolved:?}");
    assert_eq!(svc.session(pending.session()).unwrap().threads[0].resolved, None);
}

#[test]
fn resolving_a_resolved_thread_is_refused() {
    let repo = agent_branch();
    let svc = Service::new();
    let id = after_round_1(&repo, &svc);
    svc.resolve(repo.path(), &ThreadId::new("T1")).unwrap();

    let resolved = svc.resolve_thread(&ThreadRef::new(&id, &ThreadId::new("T1")));

    assert!(matches!(resolved, Err(Error::InvalidRequest(_))), "{resolved:?}");
    assert_eq!(svc.session(&id).unwrap().threads[0].resolved, Some(Party::Agent), "still the agent's resolution");
}

#[test]
fn acting_in_a_thread_once_the_session_ended_is_refused() {
    let repo = agent_branch();
    let svc = Service::new();
    let id = after_round_1(&repo, &svc);
    svc.approve(&id).unwrap();
    let t1 = ThreadRef::new(&id, &ThreadId::new("T1"));

    let reply = svc.reply_in_thread(&t1, "Thanks.");
    let resolved = svc.resolve_thread(&t1);

    assert!(matches!(reply, Err(Error::Ended)), "replying: {reply:?}");
    assert!(matches!(resolved, Err(Error::Ended)), "resolving: {resolved:?}");
}

#[test]
fn reopening_a_thread_once_the_session_ended_is_refused() {
    let repo = agent_branch();
    let svc = Service::new();
    let id = after_round_1(&repo, &svc);
    svc.resolve(repo.path(), &ThreadId::new("T1")).unwrap();
    svc.approve(&id).unwrap();

    let reopened = svc.reopen_thread(&ThreadRef::new(&id, &ThreadId::new("T1")));

    assert!(matches!(reopened, Err(Error::Ended)), "{reopened:?}");
}

#[test]
fn keeping_or_discarding_an_agent_draft_once_the_session_ended_is_refused() {
    let repo = agent_branch();
    let svc = Service::new();
    let draft = svc.draft(repo.path(), Anchor::Review, "Small.").unwrap();
    let id = svc.sessions()[0].id.clone();
    svc.close(&id).unwrap();
    let at = ThreadRef::new(&id, &draft);

    let (kept, edited, discarded) = (svc.keep_draft(&at), svc.edit_draft(&at, "Tiny."), svc.discard_draft(&at));

    assert!(matches!(kept, Err(Error::Ended)), "keeping: {kept:?}");
    assert!(matches!(edited, Err(Error::Ended)), "editing: {edited:?}");
    assert!(matches!(discarded, Err(Error::Ended)), "discarding: {discarded:?}");
    let session = svc.session(&id).expect("kept for the agent's next call");
    assert!(session.threads[0].comments[0].is_agent_draft(), "the Draft changed");
}

#[test]
fn taking_the_turn_back_from_a_pull_request_s_co_reviewer_is_refused() {
    let svc = Service::new().with_forges(vec![Box::new(FakeForge)]);
    svc.draft(PR_URL, Anchor::Review, "Small.").unwrap();
    let id = svc.sessions()[0].id.clone();

    let taken = svc.take_turn_back(&id);

    assert!(matches!(taken, Err(Error::InvalidRequest(_))), "{taken:?}");
    assert_eq!(svc.session(&id).unwrap().turn, Turn::Agent);
}

// --- Changeset views ---

fn empty() -> Changeset {
    Changeset { base: "base".into(), files: Vec::new() }
}

#[test]
fn a_session_offers_the_changes_since_the_last_round_then_since_the_session_base_then_the_full_changeset() {
    let session = SessionView {
        since_session_base: Some(empty()),
        since_last_round: Some(empty()),
        ..local(Role::Author, Turn::Human, None)
    };

    let views = session.offered_views();

    assert_eq!(views, [ChangesetView::SinceLastRound, ChangesetView::SinceSessionBase, ChangesetView::Full]);
}

#[test]
fn a_session_offers_only_the_changesets_it_has() {
    let round_2 = SessionView { since_last_round: Some(empty()), ..local(Role::Author, Turn::Human, None) };
    let round_1 = local(Role::Author, Turn::Human, None);

    let (later, first) = (round_2.offered_views(), round_1.offered_views());

    assert_eq!(later, [ChangesetView::SinceLastRound, ChangesetView::Full]);
    assert_eq!(first, [ChangesetView::Full], "only the full Changeset before a Round or a Session base");
}

// --- What a Session's tab marks it with ---

#[test]
fn attention_is_marked_before_a_reload_on_offer_and_a_reload_before_the_agents_turn() {
    let attention = SessionView { attention: true, ..local(Role::Author, Turn::Agent, None) };
    let on_disk =
        SessionView { changed_on_disk: ["lib.rs".to_owned()].into(), ..local(Role::Author, Turn::Agent, None) };
    let pushed = SessionView {
        pull_request_update: Some(PullRequestUpdate { pull_request: pr(), commits: Some(1) }),
        ..pull_request(Some(Role::CoReviewer), Turn::Agent, None)
    };
    let working = local(Role::Author, Turn::Agent, None);
    let yours = local(Role::Author, Turn::Human, None);
    let cases = [
        (
            "needs attention",
            SessionView { changed_on_disk: on_disk.changed_on_disk.clone(), ..attention },
            Some(SessionMark::Attention),
        ),
        ("changed on disk", on_disk, Some(SessionMark::ReloadOffered)),
        ("pushed to", pushed, Some(SessionMark::ReloadOffered)),
        ("the agent's Turn", working, Some(SessionMark::AgentsTurn)),
        ("the human's Turn, seen", yours, None),
    ];
    for (what, session, mark) in cases {
        assert_eq!(session.mark(), mark, "{what}");
    }
}
