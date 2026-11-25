//! Seam A: a Session of Pull Requests that holds a whole Stack, in the core review service with a
//! fake Forge adapter holding a small table of Pull Requests (ADR 0004).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use akshually_core::forge::{
    ForgeAdapter, ForgeError, ForgeKind, InboxItem, InboxRole, PullRequest, PullRequestRef, RemoteAnchor,
    RemoteComment, RemoteThread, Review,
};
use akshually_core::{
    Anchor, Changeset, Ending, Error, FileChange, InboxRowState, MemberOutcome, Outcome, PublishedReview,
    ReviewRequest, Service, SessionId, SessionView, Side, ThreadId, ThreadRef, Verdict,
};
use akshually_testkit::TestDirs;

/// A Pull Request of the fake Forge, in `acme/web`.
#[derive(Clone)]
struct Pr {
    number: u64,
    head: String,
    base: String,
    from_fork: bool,
    /// How many times it was pushed to.
    pushes: u32,
    /// It is a merge request on a GitLab-like Forge: its page has `/-/merge_requests/` in it.
    gitlab: bool,
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
    /// The Forge is GitLab-like, so its Pull Requests are `!n`.
    gitlab: bool,
    /// Every question the Forge was asked about Pull Requests, by what it was asked.
    asked: Vec<&'static str>,
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
        let mut world = self.world.lock().unwrap();
        let gitlab = world.gitlab;
        world.open.push(Pr { number, head: head.into(), base: base.into(), from_fork: false, pushes: 0, gitlab });
        drop(world);
        self
    }

    /// Opens Pull Request `number` from a fork's branch `head` into branch `base`.
    fn open_from_fork(&self, number: u64, head: &str, base: &str) -> &Self {
        let pr = Pr { number, head: head.into(), base: base.into(), from_fork: true, pushes: 0, gitlab: false };
        self.world.lock().unwrap().open.push(pr);
        self
    }

    /// The Forge retargets Pull Request `number` onto branch `base`, as it does when the one below merged.
    fn retarget(&self, number: u64, base: &str) {
        let mut world = self.world.lock().unwrap();
        world.open.iter_mut().filter(|p| p.number == number).for_each(|p| p.base = base.into());
    }

    /// Pull Request `number` receives a push.
    fn push(&self, number: u64) {
        let mut world = self.world.lock().unwrap();
        world.open.iter_mut().filter(|p| p.number == number).for_each(|p| p.pushes += 1);
    }

    /// A Forge like GitLab: its Pull Requests are merge requests, written `!n`.
    fn gitlab() -> Self {
        let forge = FakeForge::default();
        forge.world.lock().unwrap().gitlab = true;
        forge
    }

    /// How many times the Forge was asked `question` (`"head"`, `"target"` or `"pull_request"`).
    fn asked(&self, question: &str) -> usize {
        self.world.lock().unwrap().asked.iter().filter(|q| **q == question).count()
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
    let head_sha = match pr.pushes {
        0 => format!("head{}", pr.number),
        pushes => format!("head{}.{pushes}", pr.number),
    };
    PullRequest {
        id: pr_ref(pr.number),
        url: if pr.gitlab {
            format!("https://forge.test/acme/web/-/merge_requests/{}", pr.number)
        } else {
            url(pr.number)
        },
        title: format!("Pull Request {}", pr.number),
        author: "maria".into(),
        head_branch: pr.head.clone(),
        head_sha,
        base_sha: format!("base{}", pr.number),
        base_branch: pr.base.clone(),
        from_fork: pr.from_fork,
        ..Default::default()
    }
}

impl ForgeAdapter for FakeForge {
    fn host(&self) -> &str {
        "forge.test"
    }

    fn recognise(&self, url: &str) -> Option<PullRequestRef> {
        let number = url
            .strip_prefix("https://forge.test/acme/web/pull/")
            .or_else(|| url.strip_prefix("https://forge.test/acme/web/-/merge_requests/"))?;
        Some(pr_ref(number.split(['/', '#']).next()?.parse().ok()?))
    }

    fn pull_request(&self, id: &PullRequestRef) -> Result<PullRequest, ForgeError> {
        // A merged Pull Request is still there to ask for.
        let mut world = self.world.lock().unwrap();
        world.asked.push("pull_request");
        let open = world.open.iter().find(|p| p.number == id.number);
        Ok(open.map_or_else(
            || {
                to_pull_request(&Pr {
                    number: id.number,
                    head: format!("gone{}", id.number),
                    base: "main".into(),
                    from_fork: false,
                    pushes: 0,
                    gitlab: world.gitlab,
                })
            },
            to_pull_request,
        ))
    }

    fn open_with_head_branch(&self, repo: &str, branch: &str) -> Result<Vec<PullRequest>, ForgeError> {
        assert_eq!(repo, "acme/web");
        self.world.lock().unwrap().asked.push("head");
        Ok(self.lookup(|p| p.head == branch))
    }

    fn open_with_target_branch(&self, repo: &str, branch: &str) -> Result<Vec<PullRequest>, ForgeError> {
        assert_eq!(repo, "acme/web");
        self.world.lock().unwrap().asked.push("target");
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
            role: Some(InboxRole::Reviewer),
            head_branch: p.head.clone(),
            base_branch: p.base.clone(),
            from_fork: p.from_fork,
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

// --- Opening Members of a Stack that has a Session ---

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
fn a_pull_request_with_no_session_opened_below_a_stack_that_has_one_joins_that_session_as_a_new_member() {
    let forge = FakeForge::default();
    forge.open(2, "b", "a");
    let svc = forge.service();
    let own = open(&svc, 2);
    // #1 is opened under it later.
    forge.open(1, "a", "main");

    let below = open(&svc, 1);

    assert_eq!(below, own, "the Stack's Session shows it");
    assert_eq!(svc.sessions().len(), 1);
    let session = svc.session(&own).unwrap();
    assert_eq!(numbers(&session), [1, 2]);
    assert_eq!(shown(&session), 1);
}

#[test]
fn a_pull_request_opened_on_top_of_a_stack_that_has_a_session_joins_it_and_the_inbox_agrees() {
    let forge = FakeForge::default();
    forge.open(1, "a", "main");
    let svc = forge.service();
    let session = open(&svc, 1);
    forge.open(2, "b", "a");

    let on_top = open(&svc, 2);
    svc.refresh_inbox();

    assert_eq!(on_top, session);
    assert_eq!(numbers(&svc.session(&session).unwrap()), [1, 2]);
    let sessions: Vec<_> =
        svc.inbox().rows.iter().map(|r| matches!(r.state, InboxRowState::SessionOpen { .. })).collect();
    assert_eq!(sessions, [true, true], "both rows show the one Session");
}

#[test]
fn a_pull_request_between_two_sessions_joins_the_one_of_the_member_directly_below_it() {
    let forge = FakeForge::default();
    forge.open(1, "a", "main").open(3, "c", "b");
    let svc = forge.service();
    let (lower, upper) = (open(&svc, 1), open(&svc, 3));
    forge.open(2, "b", "a");

    let between = open(&svc, 2);

    assert_eq!(between, lower);
    assert_eq!(numbers(&svc.session(&lower).unwrap()), [1, 2]);
    assert_eq!(numbers(&svc.session(&upper).unwrap()), [3], "two open Sessions never merge");
}

#[test]
fn a_pull_request_whose_only_session_is_the_one_above_it_joins_that_one() {
    let forge = FakeForge::default();
    forge.open(2, "b", "x");
    let svc = forge.service();
    let upper = open(&svc, 2);
    forge.open(1, "x", "main");

    let below = open(&svc, 1);

    assert_eq!(below, upper);
    assert_eq!(numbers(&svc.session(&upper).unwrap()), [1, 2]);
}

#[test]
fn a_pull_request_that_has_its_own_session_stays_in_it_and_two_sessions_never_merge() {
    let forge = FakeForge::default();
    forge.open(1, "a", "main").open(2, "b", "other");
    let svc = forge.service();
    let (below, own) = (open(&svc, 1), open(&svc, 2));
    // The Forge now has #2 on top of #1.
    forge.retarget(2, "a");

    svc.check_pull_requests();
    let reopened = open(&svc, 2);

    assert_eq!(reopened, own);
    assert_eq!(numbers(&svc.session(&below).unwrap()), [1]);
    assert_eq!(numbers(&svc.session(&own).unwrap()), [2]);
}

#[test]
fn a_session_stops_its_stack_at_the_first_pull_request_another_session_holds() {
    let forge = FakeForge::default();
    forge.open(1, "a", "main").open(2, "b", "other");
    let svc = forge.service();
    let (a, b) = (open(&svc, 1), open(&svc, 2));
    // #2 now sits on #1, and #3 on #2: #1's Stack would run through #2 to reach #3.
    forge.retarget(2, "a");
    forge.open(3, "c", "b");

    svc.check_pull_requests();

    assert_eq!(numbers(&svc.session(&a).unwrap()), [1], "#3 is on the other side of #2, which B holds");
    assert_eq!(numbers(&svc.session(&b).unwrap()), [2, 3]);
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

    // Each Member brought one Thread from the Forge, then two Drafts.
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

fn member(number: u64, published: Option<(Verdict, &str)>) -> MemberOutcome {
    MemberOutcome {
        name: format!("acme/web #{number}"),
        url: url(number),
        verdict: published.map(|(verdict, _)| verdict),
        review: published.map(|(_, review)| review.to_owned()),
    }
}

#[test]
fn the_agent_is_told_what_became_of_each_member_whichever_members_url_it_named() {
    let svc = FakeForge::stack_of_three().service();
    let pending = svc.review(ReviewRequest::pull_request(url(2))).unwrap();
    let id = pending.session().clone();
    svc.select_member(&id, &pr_ref(1)).unwrap();
    svc.publish(&id, Verdict::Approve, "").unwrap();
    assert_eq!(pending.wait_timeout(Duration::from_millis(200)), None, "a member was Published, not the Session");

    svc.close(&id).unwrap();

    let outcome = pending.wait_timeout(Duration::from_secs(5));
    let review = "https://forge.test/acme/web/pull/1#review";
    assert_eq!(
        outcome,
        Some(Outcome::Published {
            verdict: Verdict::Approve,
            url: review.into(),
            pull_requests: vec![member(1, Some((Verdict::Approve, review))), member(2, None), member(3, None)],
        })
    );
}

#[test]
fn closing_with_no_member_published_ends_the_session_as_abandoned_for_its_agent() {
    let svc = FakeForge::stack_of_three().service();
    let pending = svc.review(ReviewRequest::pull_request(url(1))).unwrap();

    svc.close(pending.session()).unwrap();

    assert_eq!(
        pending.wait_timeout(Duration::from_secs(5)),
        Some(Outcome::Abandoned { pull_requests: vec![member(1, None), member(2, None), member(3, None)] })
    );
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

#[test]
fn a_member_whose_base_merged_stays_in_the_stack_and_keeps_getting_push_updates() {
    let forge = FakeForge::default();
    forge.open(1, "a", "main").open(2, "b", "a");
    let svc = forge.service();
    let id = open(&svc, 2);
    // #1 merges, and the Forge retargets #2 onto `main`.
    forge.close(1);
    forge.retarget(2, "main");

    svc.check_pull_requests();

    let session = svc.session(&id).unwrap();
    assert_eq!(numbers(&session), [2]);
    assert!(session.stack()[0].in_stack, "#2 is open: it did not leave the Stack");
    forge.push(2);
    svc.check_pull_requests();
    assert!(svc.session(&id).unwrap().pull_request_update.is_some(), "#2 was polled again");
}

#[test]
fn a_member_that_left_with_drafts_and_was_not_published_keeps_the_session_open() {
    let forge = FakeForge::default();
    forge.open(1, "a", "main").open(2, "b", "a");
    let svc = forge.service();
    let id = open(&svc, 1);
    svc.comment(&id, Anchor::Review, "Before it merged.").unwrap();
    forge.close(1);
    forge.retarget(2, "main");
    svc.check_pull_requests();
    svc.select_member(&id, &pr_ref(2)).unwrap();

    svc.publish(&id, Verdict::Approve, "").unwrap();

    let session = svc.session(&id).unwrap();
    assert_eq!(session.ended, None, "#1 still holds a Draft that closing would discard");
    assert_eq!(numbers(&session), [1, 2]);
}

#[test]
fn the_session_ends_as_published_when_the_last_unpublished_member_leaves_without_drafts() {
    let forge = FakeForge::default();
    forge.open(1, "a", "main").open(2, "b", "a");
    let svc = forge.service();
    let id = open(&svc, 1);
    svc.publish(&id, Verdict::Approve, "").unwrap();
    let review = "https://forge.test/acme/web/pull/1#review".to_owned();
    forge.close(2);

    svc.check_pull_requests();

    let ended = svc.session(&id).unwrap().ended;
    assert_eq!(ended, Some(Ending::Published { verdict: Verdict::Approve, url: review }));
}

#[test]
fn the_session_stays_open_when_the_last_member_leaves_and_none_was_published() {
    let forge = FakeForge::default();
    forge.open(1, "a", "main");
    let svc = forge.service();
    let id = open(&svc, 1);
    forge.close(1);

    svc.check_pull_requests();

    let session = svc.session(&id).unwrap();
    assert_eq!(session.ended, None);
    assert!(!session.stack()[0].in_stack, "it is shown as no longer part of the Stack");
}

#[test]
fn the_agent_is_told_about_a_member_that_left_with_drafts_and_one_that_was_published() {
    let forge = FakeForge::stack_of_three();
    let svc = forge.service();
    let pending = svc.review(ReviewRequest::pull_request(url(2))).unwrap();
    let id = pending.session().clone();
    svc.draft(url(1).as_str(), Anchor::Review, "From the agent.").unwrap();
    svc.select_member(&id, &pr_ref(3)).unwrap();
    svc.publish(&id, Verdict::Approve, "").unwrap();
    forge.close(1);
    forge.retarget(2, "main");
    svc.check_pull_requests();

    svc.close(&id).unwrap();

    let review = "https://forge.test/acme/web/pull/3#review";
    assert_eq!(
        pending.wait_timeout(Duration::from_secs(5)),
        Some(Outcome::Published {
            verdict: Verdict::Approve,
            url: review.into(),
            pull_requests: vec![member(1, None), member(2, None), member(3, Some((Verdict::Approve, review)))],
        })
    );
}

#[test]
fn the_ending_is_the_topmost_published_members_when_the_last_publish_was_a_lower_one() {
    let svc = FakeForge::stack_of_three().service();
    let id = open(&svc, 3);
    svc.publish(&id, Verdict::RequestChanges, "Needs work.").unwrap();
    for member in [1, 2] {
        svc.select_member(&id, &pr_ref(member)).unwrap();
        svc.publish(&id, Verdict::Approve, "").unwrap();
    }

    let ended = svc.session(&id).unwrap().ended;

    let url = "https://forge.test/acme/web/pull/3#review".to_owned();
    assert_eq!(ended, Some(Ending::Published { verdict: Verdict::RequestChanges, url }));
}

#[test]
fn on_a_gitlab_forge_the_agent_names_merge_requests_with_a_bang() {
    let forge = FakeForge::gitlab();
    forge.open(1, "a", "main").open(2, "b", "a");
    let svc = forge.service();
    let gitlab_url = |n: u64| format!("https://forge.test/acme/web/-/merge_requests/{n}");
    let pending = svc.review(ReviewRequest::pull_request(gitlab_url(1))).unwrap();
    let id = pending.session().clone();
    let thread = svc.draft(gitlab_url(2).as_str(), Anchor::Review, "On the top one.").unwrap();
    svc.reply_in_thread(&ThreadRef::new(&id, &thread), "Why?").unwrap();
    svc.hand_over_round(&id).unwrap();

    let round = pending.wait_timeout(Duration::from_secs(5));
    let again = svc.review(ReviewRequest::pull_request(gitlab_url(1))).unwrap();
    svc.close(&id).unwrap();
    let ended = again.wait_timeout(Duration::from_secs(5));

    let Some(Outcome::Round(round)) = round else { panic!("expected a Round, got {round:?}") };
    assert_eq!(round.threads[0].pull_request.as_ref().unwrap().pull_request_name, "acme/web !2");
    let Some(Outcome::Abandoned { pull_requests }) = ended else { panic!("expected Abandoned, got {ended:?}") };
    let names: Vec<_> = pull_requests.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["acme/web !1", "acme/web !2"]);
}

#[test]
fn a_check_with_no_push_and_a_recent_lookup_does_not_ask_the_forge_about_the_stack_again() {
    let forge = FakeForge::stack_of_three();
    let svc = forge.service();
    open(&svc, 2);
    svc.check_pull_requests();
    let asked = (forge.asked("head"), forge.asked("target"));

    svc.check_pull_requests();

    assert_eq!((forge.asked("head"), forge.asked("target")), asked);
}

#[test]
fn a_check_looks_the_stack_up_again_once_a_member_was_pushed_to() {
    let forge = FakeForge::stack_of_three();
    let svc = forge.service();
    open(&svc, 2);
    svc.check_pull_requests();
    let asked = forge.asked("head");
    forge.push(3);

    svc.check_pull_requests();

    assert!(forge.asked("head") > asked);
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
    // A Pull Request Session as saved before Stacks: no Members, and no target branch.
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
