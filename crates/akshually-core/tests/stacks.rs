//! Seam A: a Session of Pull Requests that holds a whole Stack, in the core review service with a
//! fake Forge adapter holding a small table of Pull Requests (ADR 0004).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use akshually_core::forge::{
    ForgeAdapter, ForgeError, ForgeKind, InboxItem, InboxRole, PullRequest, PullRequestRef, RemoteAnchor,
    RemoteComment, RemoteThread, Review,
};
use akshually_core::{
    Anchor, Changeset, Ending, Error, FileChange, InboxRowState, Outcome, PublishedReview, ReviewRequest, Service,
    SessionId, SessionView, Side, ThreadId, ThreadRef, Verdict,
};
use akshually_testkit::TestDirs;

/// A Pull Request of the fake Forge, in `acme/web`.
#[derive(Clone)]
struct Pr {
    number: u64,
    head: String,
    base: String,
    from_fork: bool,
}

#[derive(Default)]
struct World {
    open: Vec<Pr>,
    /// What was Published, by Pull Request number.
    published: Vec<(u64, Review)>,
    /// How many times the Forge was asked for Changesets.
    changesets: u32,
    /// A Forge lists this many Pull Requests at most for a branch, the first ones opened.
    page: Option<usize>,
}

/// A Forge whose open Pull Requests are the ones in `world`, which a test changes as time goes by.
#[derive(Clone, Default)]
struct FakeForge {
    world: Arc<Mutex<World>>,
}

fn pr_ref(number: u64) -> PullRequestRef {
    PullRequestRef { host: "forge.test".into(), repo: "acme/web".into(), number }
}

fn url(number: u64) -> String {
    format!("https://forge.test/acme/web/pull/{number}")
}

impl FakeForge {
    /// Opens Pull Request `number`, from branch `head` into branch `base`.
    fn open(&self, number: u64, head: &str, base: &str) -> &Self {
        let pr = Pr { number, head: head.into(), base: base.into(), from_fork: false };
        self.world.lock().unwrap().open.push(pr);
        self
    }

    /// Opens Pull Request `number` from a fork's branch `head` into branch `base`.
    fn open_from_fork(&self, number: u64, head: &str, base: &str) -> &Self {
        let pr = Pr { number, head: head.into(), base: base.into(), from_fork: true };
        self.world.lock().unwrap().open.push(pr);
        self
    }

    /// Pull Request `number` is merged, or closed: no longer open.
    fn close(&self, number: u64) {
        self.world.lock().unwrap().open.retain(|p| p.number != number);
    }

    /// A Stack `#1 <- #2 <- #3` on `main`, branches `a`, `b` and `c`.
    fn stack_of_three() -> Self {
        let forge = FakeForge::default();
        forge.open(1, "a", "main").open(2, "b", "a").open(3, "c", "b");
        forge
    }

    fn published(&self) -> Vec<(u64, Review)> {
        self.world.lock().unwrap().published.clone()
    }

    fn service(&self) -> Service {
        Service::new().with_forges(vec![Box::new(self.clone())])
    }

    fn lookup(&self, matches: impl Fn(&Pr) -> bool) -> Vec<PullRequest> {
        let world = self.world.lock().unwrap();
        let found = world.open.iter().filter(|p| matches(p)).map(to_pull_request);
        found.take(world.page.unwrap_or(usize::MAX)).collect()
    }
}

fn to_pull_request(pr: &Pr) -> PullRequest {
    PullRequest {
        id: pr_ref(pr.number),
        url: url(pr.number),
        title: format!("Pull Request {}", pr.number),
        author: "maria".into(),
        head_branch: pr.head.clone(),
        head_sha: format!("head{}", pr.number),
        base_sha: format!("base{}", pr.number),
        base_branch: pr.base.clone(),
        from_fork: pr.from_fork,
    }
}

impl ForgeAdapter for FakeForge {
    fn host(&self) -> &str {
        "forge.test"
    }

    fn recognise(&self, url: &str) -> Option<PullRequestRef> {
        let number = url.strip_prefix("https://forge.test/acme/web/pull/")?;
        Some(pr_ref(number.split(['/', '#']).next()?.parse().ok()?))
    }

    fn pull_request(&self, id: &PullRequestRef) -> Result<PullRequest, ForgeError> {
        // A merged Pull Request is still there to ask for.
        let world = self.world.lock().unwrap();
        let open = world.open.iter().find(|p| p.number == id.number);
        Ok(open.map_or_else(
            || {
                to_pull_request(&Pr {
                    number: id.number,
                    head: format!("gone{}", id.number),
                    base: "main".into(),
                    from_fork: false,
                })
            },
            to_pull_request,
        ))
    }

    fn open_with_head_branch(&self, repo: &str, branch: &str) -> Result<Vec<PullRequest>, ForgeError> {
        assert_eq!(repo, "acme/web");
        Ok(self.lookup(|p| p.head == branch))
    }

    fn open_with_target_branch(&self, repo: &str, branch: &str) -> Result<Vec<PullRequest>, ForgeError> {
        assert_eq!(repo, "acme/web");
        Ok(self.lookup(|p| p.base == branch))
    }

    fn changeset(&self, pr: &PullRequest) -> Result<Changeset, ForgeError> {
        self.world.lock().unwrap().changesets += 1;
        let path = format!("f{}.rs", pr.id.number);
        Ok(Changeset {
            base: pr.base_sha.clone(),
            files: vec![FileChange::text(&path, Some("fn a() {}\n"), Some("fn a() {\n    b();\n}\n"))],
        })
    }

    fn threads(&self, pr: &PullRequest) -> Result<Vec<RemoteThread>, ForgeError> {
        Ok(vec![RemoteThread {
            id: format!("RT_{}", pr.id.number),
            anchor: RemoteAnchor::Lines { path: format!("f{}.rs", pr.id.number), side: Side::New, start: 2, end: 2 },
            outdated: false,
            resolved_by: None,
            diff_hunk: String::new(),
            comments: vec![RemoteComment { author: "lee".into(), body: format!("Question on {}", pr.id.number) }],
        }])
    }

    fn viewer(&self) -> Result<String, ForgeError> {
        Ok("me".into())
    }

    fn verdicts(&self, _: &PullRequest, _: bool) -> Result<Vec<Verdict>, ForgeError> {
        Ok(Verdict::ALL.to_vec())
    }

    fn publish(&self, pr: &PullRequest, review: &Review) -> Result<String, ForgeError> {
        self.world.lock().unwrap().published.push((pr.id.number, review.clone()));
        Ok(format!("{}#review", pr.url))
    }

    fn inbox(&self) -> Result<Option<Vec<InboxItem>>, ForgeError> {
        let world = self.world.lock().unwrap();
        let item = |p: &Pr| InboxItem {
            id: pr_ref(p.number),
            kind: ForgeKind::GitHub,
            url: url(p.number),
            title: format!("Pull Request {}", p.number),
            author: "maria".into(),
            created_at: 0,
            updated_at: p.number as i64,
            role: InboxRole::Reviewer,
            approved: false,
            verdict_given: false,
        };
        Ok(Some(world.open.iter().map(item).collect()))
    }
}

/// The numbers of the Pull Requests the Session holds, bottom to top.
fn numbers(session: &SessionView) -> Vec<u64> {
    session.stack().iter().map(|m| m.pull_request.id.number).collect()
}

/// The number of the Pull Request the Session shows.
fn shown(session: &SessionView) -> u64 {
    session.pull_request().expect("a Pull Request Session").id.number
}

fn open(svc: &Service, number: u64) -> SessionId {
    svc.open_pull_request(&url(number)).unwrap()
}

// --- Detecting the Stack ---

#[test]
fn opening_the_middle_of_a_stack_of_three_opens_one_session_with_three_members_showing_the_middle_one() {
    let svc = FakeForge::stack_of_three().service();

    let id = open(&svc, 2);

    assert_eq!(svc.sessions().len(), 1);
    let session = svc.session(&id).unwrap();
    assert_eq!(numbers(&session), [1, 2, 3]);
    assert_eq!(shown(&session), 2);
    assert_eq!(session.changeset.files[0].path, "f2.rs");
}

#[test]
fn two_pull_requests_on_the_same_member_are_not_in_its_stack() {
    let forge = FakeForge::stack_of_three();
    forge.open(4, "d", "b");
    let svc = forge.service();

    let from_the_middle = svc.session(&open(&svc, 2)).unwrap();
    let from_one_of_them = svc.session(&open(&svc, 4)).unwrap();

    assert_eq!(numbers(&from_the_middle), [1, 2], "the Stack stops below the two on top of #2");
    // #4 was opened with #2's Session open: it is not pulled into it.
    assert_eq!(numbers(&from_one_of_them), [4]);
}

#[test]
fn opening_one_of_two_pull_requests_on_the_same_member_gives_a_stack_with_it_and_its_ancestors() {
    let forge = FakeForge::stack_of_three();
    forge.open(4, "d", "b");
    let svc = forge.service();

    let session = svc.session(&open(&svc, 4)).unwrap();

    assert_eq!(numbers(&session), [1, 2, 4]);
    assert_eq!(shown(&session), 4);
}

#[test]
fn a_pull_request_from_a_fork_whose_branch_matches_a_members_target_is_not_stacked_on_it() {
    let forge = FakeForge::default();
    // #1 is a fork's `a` branch into `main`; #2 asks to merge into the base repository's own `a`.
    forge.open_from_fork(1, "a", "main").open(2, "b", "a");

    let svc = forge.service();

    let session = svc.session(&open(&svc, 2)).unwrap();
    assert_eq!(numbers(&session), [2]);
}

#[test]
fn a_pull_request_from_a_fork_has_no_pull_request_stacked_on_its_branch_name() {
    let forge = FakeForge::default();
    forge.open_from_fork(1, "a", "main").open(2, "b", "a");
    let svc = forge.service();

    let session = svc.session(&open(&svc, 1)).unwrap();

    assert_eq!(numbers(&session), [1]);
}

#[test]
fn branch_names_that_make_a_cycle_do_not_loop_forever() {
    let forge = FakeForge::default();
    forge.open(1, "a", "b").open(2, "b", "a");
    let svc = forge.service();

    let session = svc.session(&open(&svc, 1)).unwrap();

    assert_eq!(numbers(&session), [2, 1]);
}

#[test]
fn a_pull_request_with_no_neighbours_is_a_stack_of_one() {
    let forge = FakeForge::default();
    forge.open(9, "solo", "main");
    let svc = forge.service();

    let session = svc.session(&open(&svc, 9)).unwrap();

    assert_eq!(numbers(&session), [9]);
    assert_eq!(session.stack()[0].drafts, 0);
    assert!(session.stack()[0].in_stack && session.stack()[0].selected);
}

// --- Opening members of a Stack that has a Session ---

#[test]
fn opening_another_member_shows_the_existing_session_on_that_member() {
    let svc = FakeForge::stack_of_three().service();
    let id = open(&svc, 2);

    let again = open(&svc, 3);

    assert_eq!(again, id);
    assert_eq!(svc.sessions().len(), 1);
    assert_eq!(shown(&svc.session(&id).unwrap()), 3);
    assert!(svc.window().displayed.is_session(&id));
}

#[test]
fn a_pull_request_with_its_own_session_is_not_pulled_into_another_sessions_stack() {
    let forge = FakeForge::default();
    forge.open(2, "b", "a");
    let svc = forge.service();
    let own = open(&svc, 2);
    // #1 is opened under it later.
    forge.open(1, "a", "main");

    let below = open(&svc, 1);

    assert_ne!(below, own);
    assert_eq!(numbers(&svc.session(&below).unwrap()), [1]);
    assert_eq!(numbers(&svc.session(&own).unwrap()), [2]);
}

#[test]
fn a_session_does_not_take_a_pull_request_with_its_own_session_when_the_stack_is_refreshed() {
    let forge = FakeForge::default();
    forge.open(1, "a", "main");
    let svc = forge.service();
    let below = open(&svc, 1);
    forge.open(2, "b", "a");
    let own = open(&svc, 2);
    assert_ne!(own, below, "the Pull Request was not there when #1's Session opened");

    svc.check_pull_requests();

    assert_eq!(numbers(&svc.session(&below).unwrap()), [1]);
    assert_eq!(numbers(&svc.session(&own).unwrap()), [2]);
}

// --- Members keep their own state ---

#[test]
fn drafts_threads_and_ticks_are_kept_per_member_when_switching_back_and_forth() {
    let svc = FakeForge::stack_of_three().service();
    let id = open(&svc, 1);
    svc.comment(&id, Anchor::Review, "On the first.").unwrap();
    svc.mark_viewed(&id, "f1.rs", true).unwrap();

    svc.select_member(&id, &pr_ref(3)).unwrap();
    let top = svc.session(&id).unwrap();
    assert_eq!((top.drafts(), top.viewed.len(), top.changeset.files[0].path.as_str()), (0, 0, "f3.rs"));
    svc.comment(&id, Anchor::Review, "On the third.").unwrap();
    svc.comment(&id, Anchor::Review, "More on the third.").unwrap();
    svc.select_member(&id, &pr_ref(1)).unwrap();

    let bottom = svc.session(&id).unwrap();
    assert_eq!(shown(&bottom), 1);
    assert_eq!(bottom.drafts(), 1);
    assert!(bottom.viewed.contains("f1.rs"));
    let drafts: Vec<_> = bottom.stack().iter().map(|m| m.drafts).collect();
    assert_eq!(drafts, [1, 0, 2]);
}

#[test]
fn switching_members_moves_the_changeset_revision_so_the_diff_is_rebuilt() {
    let svc = FakeForge::stack_of_three().service();
    let id = open(&svc, 1);
    let before = svc.session(&id).unwrap().changeset_rev;

    svc.select_member(&id, &pr_ref(2)).unwrap();

    assert_ne!(svc.session(&id).unwrap().changeset_rev, before);
}

#[test]
fn selecting_a_pull_request_outside_the_stack_is_refused() {
    let svc = FakeForge::stack_of_three().service();
    let id = open(&svc, 1);

    let refused = svc.select_member(&id, &pr_ref(99));

    assert!(matches!(refused, Err(Error::NotInStack(_))), "{refused:?}");
    assert_eq!(shown(&svc.session(&id).unwrap()), 1);
}

#[test]
fn thread_ids_are_unique_across_the_members_of_a_session() {
    let svc = FakeForge::stack_of_three().service();
    let id = open(&svc, 2);
    svc.select_member(&id, &pr_ref(1)).unwrap();
    let new = svc.comment(&id, Anchor::Review, "Mine.").unwrap();

    svc.select_member(&id, &pr_ref(3)).unwrap();
    let other = svc.comment(&id, Anchor::Review, "Another.").unwrap();

    // Each member brought one Thread from the Forge, then two Drafts.
    let mut ids: Vec<ThreadId> = Vec::new();
    for member in [1, 2, 3] {
        svc.select_member(&id, &pr_ref(member)).unwrap();
        ids.extend(svc.session(&id).unwrap().threads.iter().map(|t| t.id.clone()));
    }
    let mut unique = ids.clone();
    unique.sort_by_key(|t| t.to_string());
    unique.dedup();
    assert_eq!(unique.len(), 5, "{ids:?}");
    assert_ne!(new, other);
}

// --- Publish and Ending ---

#[test]
fn publishing_one_member_sends_only_its_drafts_and_verdict_and_leaves_the_session_open() {
    let forge = FakeForge::stack_of_three();
    let svc = forge.service();
    let id = open(&svc, 1);
    svc.comment(&id, Anchor::Review, "For the first.").unwrap();
    svc.select_member(&id, &pr_ref(2)).unwrap();
    svc.comment(&id, Anchor::Review, "For the second.").unwrap();

    let url = svc.publish(&id, Verdict::RequestChanges, "Please fix.").unwrap();

    assert_eq!(url, "https://forge.test/acme/web/pull/2#review");
    let published = forge.published();
    assert_eq!(published.len(), 1);
    let (number, review) = &published[0];
    assert_eq!((*number, review.verdict), (2, Verdict::RequestChanges));
    assert_eq!(review.body, "Please fix.\n\nFor the second.");
    let session = svc.session(&id).unwrap();
    assert!(session.ended.is_none(), "the Session goes on with the other members");
    let stack = session.stack();
    assert_eq!(
        stack[1].published,
        Some(PublishedReview {
            verdict: Verdict::RequestChanges,
            url: "https://forge.test/acme/web/pull/2#review".into()
        })
    );
    assert_eq!((stack[0].published.clone(), stack[0].drafts), (None, 1), "the first member keeps its Draft");
}

#[test]
fn a_published_member_takes_no_more_drafts() {
    let svc = FakeForge::stack_of_three().service();
    let id = open(&svc, 1);
    svc.publish(&id, Verdict::Approve, "").unwrap();

    let refused = svc.comment(&id, Anchor::Review, "Too late.");
    let agent = svc.draft(url(1).as_str(), Anchor::Review, "Too late too.");

    assert!(matches!(refused, Err(Error::AlreadyPublished)), "{refused:?}");
    assert!(matches!(agent, Err(Error::AlreadyPublished)), "{agent:?}");
    // The others still take them.
    svc.select_member(&id, &pr_ref(2)).unwrap();
    svc.comment(&id, Anchor::Review, "Still fine.").unwrap();
}

#[test]
fn a_published_member_cannot_be_published_again() {
    let svc = FakeForge::stack_of_three().service();
    let id = open(&svc, 1);
    svc.publish(&id, Verdict::Approve, "").unwrap();

    let again = svc.publish(&id, Verdict::Approve, "");

    assert!(matches!(again, Err(Error::AlreadyPublished)), "{again:?}");
}

#[test]
fn publishing_the_last_unpublished_member_ends_the_session_as_published() {
    let svc = FakeForge::stack_of_three().service();
    let id = open(&svc, 1);
    for member in [1, 2] {
        svc.select_member(&id, &pr_ref(member)).unwrap();
        svc.publish(&id, Verdict::Approve, "").unwrap();
        assert!(svc.session(&id).unwrap().ended.is_none());
    }
    svc.select_member(&id, &pr_ref(3)).unwrap();

    svc.publish(&id, Verdict::Feedback, "Last.").unwrap();

    let ended = svc.session(&id).unwrap().ended;
    let url = "https://forge.test/acme/web/pull/3#review".to_owned();
    assert_eq!(ended, Some(Ending::Published { verdict: Verdict::Feedback, url }));
}

#[test]
fn the_agent_is_told_abandoned_when_the_pull_request_its_url_names_was_not_published() {
    let svc = FakeForge::stack_of_three().service();
    let pending = svc.review(ReviewRequest::pull_request(url(2))).unwrap();
    let id = pending.session().clone();
    svc.select_member(&id, &pr_ref(1)).unwrap();
    svc.publish(&id, Verdict::Approve, "").unwrap();

    svc.close(&id).unwrap();

    let outcome = pending.wait_timeout(Duration::from_secs(5));
    assert_eq!(outcome, Some(Outcome::Abandoned), "#2, the Pull Request the call named, wasn't Published");
}

#[test]
fn the_agent_is_told_the_outcome_of_the_pull_request_its_url_names() {
    let svc = FakeForge::stack_of_three().service();
    let pending = svc.review(ReviewRequest::pull_request(url(1))).unwrap();
    let id = pending.session().clone();
    svc.publish(&id, Verdict::RequestChanges, "Needs work.").unwrap();
    svc.select_member(&id, &pr_ref(2)).unwrap();
    assert_eq!(pending.wait_timeout(Duration::from_millis(200)), None, "a member was Published, not the Session");

    svc.close(&id).unwrap();

    let outcome = pending.wait_timeout(Duration::from_secs(5));
    let url = "https://forge.test/acme/web/pull/1#review".to_owned();
    assert_eq!(outcome, Some(Outcome::Published { verdict: Verdict::RequestChanges, url }));
}

#[test]
fn closing_with_no_member_published_ends_the_session_as_abandoned_for_its_agent() {
    let svc = FakeForge::stack_of_three().service();
    let pending = svc.review(ReviewRequest::pull_request(url(1))).unwrap();

    svc.close(pending.session()).unwrap();

    assert_eq!(pending.wait_timeout(Duration::from_secs(5)), Some(Outcome::Abandoned));
}

#[test]
fn closing_asks_for_confirmation_only_when_a_member_holds_drafts_or_an_agent_is_there() {
    let svc = FakeForge::stack_of_three().service();
    let id = open(&svc, 1);
    assert_eq!(svc.session(&id).unwrap().actions().end, akshually_core::EndAction::Close);
    svc.select_member(&id, &pr_ref(3)).unwrap();
    svc.comment(&id, Anchor::Review, "A Draft on the top one.").unwrap();
    svc.select_member(&id, &pr_ref(1)).unwrap();

    let end = svc.session(&id).unwrap().actions().end;

    assert_eq!(end, akshually_core::EndAction::Abandon, "the Draft is on a member that isn't shown");
}

// --- A Stack that changes ---

#[test]
fn refreshing_a_stack_that_did_not_change_leaves_its_session_as_it_was() {
    let forge = FakeForge::stack_of_three();
    let svc = forge.service();
    let id = open(&svc, 2);
    svc.comment(&id, Anchor::Review, "A Draft.").unwrap();
    let before = svc.session(&id).unwrap();

    svc.check_pull_requests();

    assert_eq!(svc.session(&id).unwrap(), before);
}

#[test]
fn a_pull_request_into_a_busy_branch_stays_when_the_forge_lists_only_the_first_of_them() {
    let forge = FakeForge::default();
    // Three open Pull Requests into `main` before #99, and the Forge lists the first two.
    forge.open(10, "x", "main").open(11, "y", "main").open(12, "z", "main").open(99, "solo", "main");
    forge.world.lock().unwrap().page = Some(2);
    let svc = forge.service();
    let id = open(&svc, 99);
    let before = svc.session(&id).unwrap();

    svc.check_pull_requests();

    assert_eq!(svc.session(&id).unwrap(), before, "#99 left its Stack because it is on a later page");
    assert!(svc.session(&id).unwrap().stack()[0].in_stack);
}

#[test]
fn a_new_pull_request_on_top_of_the_stack_joins_the_session_when_refreshed() {
    let forge = FakeForge::stack_of_three();
    let svc = forge.service();
    let id = open(&svc, 2);
    svc.comment(&id, Anchor::Review, "Kept.").unwrap();
    forge.open(4, "d", "c");

    svc.check_pull_requests();

    let session = svc.session(&id).unwrap();
    assert_eq!(numbers(&session), [1, 2, 3, 4]);
    assert_eq!((shown(&session), session.drafts()), (2, 1), "the member shown stays, with its Draft");
}

#[test]
fn a_merged_member_with_drafts_stays_marked_as_no_longer_in_the_stack() {
    let forge = FakeForge::stack_of_three();
    let svc = forge.service();
    let id = open(&svc, 3);
    svc.select_member(&id, &pr_ref(1)).unwrap();
    svc.comment(&id, Anchor::Review, "Before it merged.").unwrap();
    svc.select_member(&id, &pr_ref(3)).unwrap();
    forge.close(1);

    svc.check_pull_requests();

    let session = svc.session(&id).unwrap();
    let stack = session.stack();
    let in_stack: Vec<_> = stack.iter().map(|m| (m.pull_request.id.number, m.in_stack)).collect();
    assert_eq!(in_stack, [(1, false), (2, true), (3, true)]);
    assert_eq!(shown(&session), 3);
}

#[test]
fn a_merged_member_with_no_drafts_disappears() {
    let forge = FakeForge::stack_of_three();
    let svc = forge.service();
    let id = open(&svc, 3);
    forge.close(1);

    svc.check_pull_requests();

    let session = svc.session(&id).unwrap();
    assert_eq!(numbers(&session), [2, 3]);
    assert_eq!(shown(&session), 3);
}

#[test]
fn a_published_member_that_left_the_stack_stays() {
    let forge = FakeForge::stack_of_three();
    let svc = forge.service();
    let id = open(&svc, 1);
    svc.publish(&id, Verdict::Approve, "").unwrap();
    forge.close(1);

    svc.check_pull_requests();

    let session = svc.session(&id).unwrap();
    assert_eq!(numbers(&session), [1, 2, 3]);
    assert!(!session.stack()[0].in_stack && session.stack()[0].published.is_some());
}

#[test]
fn when_the_member_shown_leaves_the_stack_the_session_shows_one_that_remains() {
    let forge = FakeForge::stack_of_three();
    let svc = forge.service();
    let id = open(&svc, 1);
    forge.close(1);

    svc.check_pull_requests();

    let session = svc.session(&id).unwrap();
    assert_eq!(numbers(&session), [2, 3]);
    assert_eq!(shown(&session), 2);
}

#[test]
fn the_session_ends_once_every_member_still_in_the_stack_is_published() {
    let forge = FakeForge::stack_of_three();
    let svc = forge.service();
    let id = open(&svc, 1);
    svc.publish(&id, Verdict::Approve, "").unwrap();
    forge.close(1);
    svc.check_pull_requests();
    for member in [2, 3] {
        svc.select_member(&id, &pr_ref(member)).unwrap();
        svc.publish(&id, Verdict::Approve, "").unwrap();
    }

    assert!(matches!(svc.session(&id).unwrap().ended, Some(Ending::Published { .. })));
}

// --- The agent ---

#[test]
fn a_draft_added_with_the_url_of_a_member_is_in_that_members_review() {
    let svc = FakeForge::stack_of_three().service();
    let id = open(&svc, 1);

    let thread = svc.draft(url(2).as_str(), Anchor::Review, "From the agent.").unwrap();

    let session = svc.session(&id).unwrap();
    assert_eq!(shown(&session), 1, "the human's view of the Stack stays where it was");
    let drafts: Vec<_> = session.stack().iter().map(|m| m.drafts).collect();
    assert_eq!(drafts, [0, 1, 0]);
    svc.select_member(&id, &pr_ref(2)).unwrap();
    assert!(svc.session(&id).unwrap().threads.iter().any(|t| t.id == thread));
}

#[test]
fn an_agents_review_on_one_members_url_reaches_the_session_of_the_stack() {
    let svc = FakeForge::stack_of_three().service();
    let id = open(&svc, 1);

    let pending = svc.review(ReviewRequest::pull_request(url(3))).unwrap();

    assert_eq!(pending.session(), &id);
    assert_eq!(svc.sessions().len(), 1);
}

#[test]
fn the_agents_drafts_on_every_member_are_handed_over_with_the_questions_on_them() {
    let svc = FakeForge::stack_of_three().service();
    let id = open(&svc, 1);
    let thread = svc.draft(url(3).as_str(), Anchor::Review, "Agent's note on the top.").unwrap();
    svc.reply_in_thread(&ThreadRef::new(&id, &thread), "Why?").unwrap();
    let pending = svc.review(ReviewRequest::pull_request(url(1))).unwrap();

    svc.hand_over_round(&id).unwrap();

    let Some(Outcome::Round(round)) = pending.wait_timeout(Duration::from_secs(5)) else { panic!("no Round") };
    assert_eq!(round.threads.len(), 1, "the question on the member not shown goes too");
    assert_eq!(round.threads[0].id, thread);
}

#[test]
fn replying_and_resolving_through_the_url_of_a_member_reach_its_thread_while_another_is_shown() {
    let svc = FakeForge::stack_of_three().service();
    let id = open(&svc, 1);
    let thread = svc.draft(url(3).as_str(), Anchor::Review, "Agent's note on the top.").unwrap();
    svc.reply_in_thread(&ThreadRef::new(&id, &thread), "Why?").unwrap();
    let pending = svc.review(ReviewRequest::pull_request(url(3))).unwrap();
    svc.hand_over_round(&id).unwrap();
    pending.wait_timeout(Duration::from_secs(5)).expect("the Round");

    svc.reply(url(3).as_str(), &thread, "Because.").unwrap();
    svc.resolve(url(3).as_str(), &thread).unwrap();

    assert_eq!(shown(&svc.session(&id).unwrap()), 1);
    svc.select_member(&id, &pr_ref(3)).unwrap();
    let top = svc.session(&id).unwrap();
    let found = top.threads.iter().find(|t| t.id == thread).expect("the Thread is in the top member");
    assert_eq!(found.comments.last().map(|c| c.body.as_str()), Some("Because."));
    assert!(found.resolved.is_some());
}

// --- The Inbox ---

#[test]
fn every_inbox_row_of_a_member_shows_the_session_open_with_that_members_drafts() {
    let svc = FakeForge::stack_of_three().service();
    let id = open(&svc, 1);
    svc.comment(&id, Anchor::Review, "On the first.").unwrap();
    svc.select_member(&id, &pr_ref(3)).unwrap();
    svc.comment(&id, Anchor::Review, "On the third.").unwrap();
    svc.comment(&id, Anchor::Review, "Again.").unwrap();

    svc.refresh_inbox();

    let rows: Vec<_> = svc.inbox().rows.into_iter().map(|r| (r.item.id.number, r.state)).collect();
    let open = |drafts| InboxRowState::SessionOpen { session: id.clone(), drafts };
    assert!(rows.contains(&(1, open(1))), "{rows:?}");
    assert!(rows.contains(&(2, open(0))), "{rows:?}");
    assert!(rows.contains(&(3, open(2))), "{rows:?}");
}

// --- Sessions saved before Stacks, and the ones saved with them ---

#[test]
fn a_session_saved_before_stacks_is_restored_as_a_stack_of_one() {
    let dirs = TestDirs::new();
    let sessions = dirs.state().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    // A Pull Request Session as saved before Stacks: no members, and no target branch.
    let saved = r#"{"view":{"id":"s1","source":{"pull_request":{"id":{"host":"forge.test","repo":"acme/web","number":5},
        "url":"https://forge.test/acme/web/pull/5","title":"Old","author":"maria","head_branch":"old",
        "head_sha":"head5","base_sha":"base5"}},"round":1}}"#;
    std::fs::write(sessions.join("s1.json"), saved).unwrap();

    let svc = Service::open(dirs.state()).unwrap();

    let session = svc.session(&SessionId::new("s1")).unwrap();
    assert_eq!(numbers(&session), [5]);
    assert!(session.stack()[0].selected && session.stack()[0].in_stack);
    assert_eq!(session.stack()[0].published, None);
}

#[test]
fn a_stack_session_comes_back_after_a_restart_with_its_members_and_their_drafts() {
    let dirs = TestDirs::new();
    let forge = FakeForge::stack_of_three();
    let svc = Service::open(dirs.state()).unwrap().with_forges(vec![Box::new(forge.clone())]);
    let id = open(&svc, 2);
    svc.comment(&id, Anchor::Review, "On the second.").unwrap();
    svc.select_member(&id, &pr_ref(3)).unwrap();
    svc.publish(&id, Verdict::Approve, "").unwrap();
    let before = svc.session(&id).unwrap();
    drop(svc);

    let restored = Service::open(dirs.state()).unwrap();

    assert_eq!(restored.session(&id).unwrap(), before);
    let drafts: Vec<_> = restored.session(&id).unwrap().stack().iter().map(|m| m.drafts).collect();
    assert_eq!(drafts, [0, 1, 0]);
}

#[test]
fn a_stack_of_one_asks_the_forge_for_one_changeset() {
    let forge = FakeForge::default();
    forge.open(9, "solo", "main");
    let svc = forge.service();

    open(&svc, 9);

    assert_eq!(forge.world.lock().unwrap().changesets, 1);
}
