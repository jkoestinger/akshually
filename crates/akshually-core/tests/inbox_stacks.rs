//! Seam A: the Inbox grouping a Stack's Pull Requests, in the core review service with a fake
//! Forge adapter holding a small table of Pull Requests (ADR 0004).

use std::sync::{Arc, Mutex};

use akshually_core::forge::{
    ForgeAdapter, ForgeError, ForgeKind, InboxItem, InboxRole, PullRequest, PullRequestRef, RemoteThread, Review,
};
use akshually_core::{Changeset, FileChange, InboxRow, InboxRowState, InboxStanding, Service, Verdict};

/// A Pull Request of the fake Forge, in `acme/web`, and how it stands in the human's Inbox.
#[derive(Clone)]
struct Pr {
    number: u64,
    head: String,
    base: String,
    updated_at: i64,
    /// `None` when the Forge doesn't list it for the human.
    listed: Option<InboxRole>,
    verdict_given: bool,
    approved: bool,
}

#[derive(Default)]
struct World {
    open: Vec<Pr>,
    /// The Stack lookup fails.
    lookup_fails: bool,
    /// The Forge fails to list the Inbox.
    inbox_fails: bool,
    /// How many times the Forge was asked for a Pull Request by its number.
    pull_requests_asked: u32,
}

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
    /// Opens Pull Request `number` from branch `head` into `base`, which the Forge doesn't list.
    fn open(&self, number: u64, head: &str, base: &str) -> &Self {
        let pr = Pr {
            number,
            head: head.into(),
            base: base.into(),
            updated_at: 0,
            listed: None,
            verdict_given: false,
            approved: false,
        };
        self.world.lock().unwrap().open.push(pr);
        self
    }

    fn change(&self, number: u64, f: impl FnOnce(&mut Pr)) {
        let mut world = self.world.lock().unwrap();
        f(world.open.iter_mut().find(|p| p.number == number).expect("an open Pull Request"));
    }

    /// The human's review is requested on `number`, which was last updated at `updated_at`.
    fn request(&self, number: u64, updated_at: i64) -> &Self {
        self.change(number, |p| {
            p.listed = Some(InboxRole::Reviewer);
            p.verdict_given = false;
            p.updated_at = updated_at;
        });
        self
    }

    /// The Forge lists `number` as one the human gave a Verdict on.
    fn verdict_given(&self, number: u64, updated_at: i64) -> &Self {
        self.change(number, |p| {
            p.listed = Some(InboxRole::Reviewer);
            p.verdict_given = true;
            p.updated_at = updated_at;
        });
        self
    }

    /// The Forge lists `number` as one that is approved already.
    fn approved(&self, number: u64, updated_at: i64) -> &Self {
        self.change(number, |p| {
            p.listed = Some(InboxRole::Reviewer);
            p.approved = true;
            p.updated_at = updated_at;
        });
        self
    }

    /// `#1 <- #2 <- #3` on `main`, branches `a`, `b` and `c`.
    fn stack_of_three() -> Self {
        let forge = FakeForge::default();
        forge.open(1, "a", "main").open(2, "b", "a").open(3, "c", "b");
        forge
    }

    fn service(&self) -> Service {
        Service::new().with_forges(vec![Box::new(self.clone())])
    }

    fn lookup(&self, matches: impl Fn(&Pr) -> bool) -> Result<Vec<PullRequest>, ForgeError> {
        let world = self.world.lock().unwrap();
        if world.lookup_fails {
            return Err(ForgeError::Http { host: "forge.test".into(), message: "lookup down".into() });
        }
        Ok(world.open.iter().filter(|p| matches(p)).map(to_pull_request).collect())
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
        from_fork: false,
        created_at: 1_000,
        updated_at: pr.updated_at,
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
        let mut world = self.world.lock().unwrap();
        world.pull_requests_asked += 1;
        let open = world.open.iter().find(|p| p.number == id.number);
        Ok(to_pull_request(open.expect("an open Pull Request")))
    }

    fn open_with_head_branch(&self, _: &str, branch: &str) -> Result<Vec<PullRequest>, ForgeError> {
        self.lookup(|p| p.head == branch)
    }

    fn open_with_target_branch(&self, _: &str, branch: &str) -> Result<Vec<PullRequest>, ForgeError> {
        self.lookup(|p| p.base == branch)
    }

    fn changeset(&self, pr: &PullRequest) -> Result<Changeset, ForgeError> {
        let path = format!("f{}.rs", pr.id.number);
        Ok(Changeset { base: pr.base_sha.clone(), files: vec![FileChange::text(&path, Some("a\n"), Some("b\n"))] })
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

    fn publish(&self, pr: &PullRequest, _: &Review) -> Result<String, ForgeError> {
        Ok(format!("{}#review", pr.url))
    }

    fn inbox(&self) -> Result<Option<Vec<InboxItem>>, ForgeError> {
        let world = self.world.lock().unwrap();
        if world.inbox_fails {
            return Err(ForgeError::Http { host: "forge.test".into(), message: "inbox down".into() });
        }
        let item = |p: &Pr| {
            Some(InboxItem {
                id: pr_ref(p.number),
                kind: ForgeKind::GitHub,
                url: url(p.number),
                title: format!("Pull Request {}", p.number),
                author: "maria".into(),
                created_at: 1_000,
                updated_at: p.updated_at,
                role: Some(p.listed?),
                head_branch: p.head.clone(),
                base_branch: p.base.clone(),
                from_fork: false,
                approved: p.approved,
                verdict_given: p.verdict_given,
            })
        };
        Ok(Some(world.open.iter().filter_map(item).collect()))
    }
}

/// A row as `(number, position in its Stack and the Stack's size, qualifies)`.
type Shape = (u64, Option<(usize, usize)>, bool);

fn shape(svc: &Service) -> Vec<Shape> {
    let place = |r: &InboxRow| r.stack.as_ref().map(|s| (s.position, s.size));
    svc.inbox().rows.iter().map(|r| (r.item.id.number, place(r), r.qualifies())).collect()
}

fn numbers(svc: &Service) -> Vec<u64> {
    svc.inbox().rows.iter().map(|r| r.item.id.number).collect()
}

#[test]
fn a_stack_asked_for_on_its_top_member_alone_is_shown_whole_bottom_first_with_the_others_marked() {
    let forge = FakeForge::stack_of_three();
    forge.request(3, 300);
    let svc = forge.service();

    svc.refresh_inbox();

    assert_eq!(
        shape(&svc),
        [(1, Some((0, 3)), false), (2, Some((1, 3)), false), (3, Some((2, 3)), true)],
        "the group in Stack order, only the top member asked for"
    );
}

#[test]
fn opening_a_member_that_was_not_requested_opens_the_stacks_session_on_it() {
    let forge = FakeForge::stack_of_three();
    forge.request(3, 300);
    let svc = forge.service();
    svc.refresh_inbox();

    let id = svc.open_pull_request(&svc.inbox().rows[0].item.url).unwrap();

    let session = svc.session(&id).unwrap();
    assert_eq!(session.stack().iter().map(|m| m.pull_request.id.number).collect::<Vec<_>>(), [1, 2, 3]);
    assert_eq!(session.pull_request().unwrap().id.number, 1);
    let states: Vec<_> = svc.inbox().rows.iter().map(|r| r.state.clone()).collect();
    assert!(states.iter().all(|s| matches!(s, InboxRowState::SessionOpen { .. })), "{states:?}");
}

#[test]
fn each_member_keeps_its_own_row_state() {
    let forge = FakeForge::stack_of_three();
    forge.request(3, 300);
    let svc = forge.service();
    svc.refresh_inbox();

    let id = svc.open_pull_request(&url(3)).unwrap();
    svc.close(&id).unwrap();
    svc.refresh_inbox();

    let states: Vec<_> = svc.inbox().rows.iter().map(|r| r.state.clone()).collect();
    assert_eq!(states, [InboxRowState::New, InboxRowState::New, InboxRowState::Seen]);
}

#[test]
fn a_member_the_human_gave_a_verdict_on_stays_in_the_group_marked_while_another_member_qualifies() {
    let forge = FakeForge::stack_of_three();
    forge.verdict_given(1, 100).request(2, 200);
    let svc = forge.service();

    svc.refresh_inbox();

    assert_eq!(shape(&svc), [(1, Some((0, 3)), false), (2, Some((1, 3)), true), (3, Some((2, 3)), false)]);
}

#[test]
fn a_group_none_of_whose_members_qualify_is_gone_unless_a_session_is_open() {
    let forge = FakeForge::stack_of_three();
    forge.verdict_given(1, 100).request(2, 200);
    let svc = forge.service();
    svc.refresh_inbox();
    forge.verdict_given(2, 200);

    svc.refresh_inbox();
    assert!(svc.inbox().rows.is_empty(), "no member qualifies and no Session is open");

    svc.open_pull_request(&url(2)).unwrap();
    svc.refresh_inbox();
    assert_eq!(numbers(&svc), [1, 2, 3], "an open Session keeps the Stack in the Inbox");
}

#[test]
fn stacks_and_lone_pull_requests_are_ordered_by_their_most_recently_updated_qualifying_member() {
    let forge = FakeForge::default();
    // Stack #1 <- #2 with #1 at 200; a lone #5 at 300; Stack #3 <- #4 with #4 at 400.
    forge.open(1, "a", "main").open(2, "b", "a").open(5, "e", "main").open(3, "c", "main").open(4, "d", "c");
    forge.request(1, 200).request(5, 300).request(4, 400).request(2, 50);
    let svc = forge.service();

    svc.refresh_inbox();

    assert_eq!(numbers(&svc), [3, 4, 5, 1, 2]);
}

#[test]
fn a_lone_pull_request_is_a_plain_row() {
    let forge = FakeForge::default();
    forge.open(1, "a", "main").request(1, 100);
    let svc = forge.service();

    svc.refresh_inbox();

    assert_eq!(shape(&svc), [(1, None, true)]);
}

#[test]
fn a_stack_none_of_whose_members_qualify_is_not_shown() {
    let forge = FakeForge::stack_of_three();
    let svc = forge.service();

    svc.refresh_inbox();

    assert!(svc.inbox().rows.is_empty());
}

#[test]
fn a_push_to_a_member_that_does_not_qualify_does_not_mark_the_inbox_but_a_review_request_on_it_does() {
    let forge = FakeForge::stack_of_three();
    forge.verdict_given(1, 100).request(3, 300);
    let svc = forge.service();
    svc.refresh_inbox();
    // The human saw the Inbox, then went into a Session and came back out of it: the Window falls
    // back to the Inbox without the human opening it.
    svc.display_inbox();
    let id = svc.open_pull_request(&url(3)).unwrap();
    svc.close(&id).unwrap();
    svc.refresh_inbox();
    assert!(!svc.inbox().attention);

    forge.change(1, |p| p.updated_at = 500);
    svc.refresh_inbox();
    assert!(!svc.inbox().attention, "a push to a member that doesn't qualify");

    forge.request(1, 500);
    svc.refresh_inbox();
    assert!(svc.inbox().attention, "a new review request on a member");
}

#[test]
fn a_failed_stack_lookup_leaves_the_rows_ungrouped_and_reports_the_error() {
    let forge = FakeForge::stack_of_three();
    forge.request(3, 300).request(1, 100);
    forge.world.lock().unwrap().lookup_fails = true;
    let svc = forge.service();

    svc.refresh_inbox();

    let view = svc.inbox();
    assert_eq!(shape(&svc), [(3, None, true), (1, None, true)]);
    assert_eq!(view.errors.len(), 1, "{:?}", view.errors);
    assert!(view.errors[0].contains("lookup down"), "{:?}", view.errors);
}

#[test]
fn a_group_takes_the_place_of_its_newest_qualifying_member_not_of_one_a_session_keeps_listed() {
    let forge = FakeForge::stack_of_three();
    forge.open(5, "e", "main");
    forge.verdict_given(1, 900).request(2, 200).request(5, 500);
    let svc = forge.service();
    svc.refresh_inbox();
    svc.open_pull_request(&url(2)).unwrap();

    svc.refresh_inbox();

    // #1 stays listed for the Session on its Stack, and is the newest, but doesn't qualify.
    assert_eq!(numbers(&svc), [5, 1, 2, 3]);
}

#[test]
fn a_host_that_fails_to_list_keeps_its_groups_from_before() {
    let forge = FakeForge::stack_of_three();
    forge.request(3, 300);
    let svc = forge.service();
    svc.refresh_inbox();
    forge.world.lock().unwrap().inbox_fails = true;

    svc.refresh_inbox();

    let view = svc.inbox();
    assert_eq!(shape(&svc), [(1, Some((0, 3)), false), (2, Some((1, 3)), false), (3, Some((2, 3)), true)]);
    assert!(view.errors[0].contains("inbox down"), "{:?}", view.errors);
}

#[test]
fn a_refresh_asks_the_forge_for_no_pull_request_because_the_listing_says_where_each_one_sits() {
    let forge = FakeForge::stack_of_three();
    forge.request(3, 300).request(1, 100);
    let svc = forge.service();

    svc.refresh_inbox();

    assert_eq!(forge.world.lock().unwrap().pull_requests_asked, 0);
    assert_eq!(numbers(&svc), [1, 2, 3], "and the Stack is grouped all the same");
}

#[test]
fn a_member_the_forge_did_not_list_has_no_role_and_the_times_the_forge_gave() {
    let forge = FakeForge::stack_of_three();
    forge.request(3, 300);
    forge.change(1, |p| p.updated_at = 150);
    let svc = forge.service();

    svc.refresh_inbox();

    let rows = svc.inbox().rows;
    let unlisted = &rows[0].item;
    assert_eq!((unlisted.id.number, unlisted.role), (1, None));
    assert_eq!((unlisted.created_at, unlisted.updated_at), (1_000, 150));
    assert_eq!(rows[2].item.role, Some(InboxRole::Reviewer));
}

#[test]
fn a_member_the_forge_did_not_list_shows_as_updated_once_the_forge_says_it_changed() {
    let forge = FakeForge::stack_of_three();
    forge.request(3, 300);
    forge.change(1, |p| p.updated_at = 150);
    let svc = forge.service();
    svc.refresh_inbox();
    let id = svc.open_pull_request(&url(1)).unwrap();
    svc.close(&id).unwrap();
    svc.refresh_inbox();
    assert_eq!(svc.inbox().rows[0].state, InboxRowState::Seen);

    forge.change(1, |p| p.updated_at = 600);
    svc.refresh_inbox();

    assert_eq!(svc.inbox().rows[0].state, InboxRowState::Updated);
}

#[test]
fn a_row_says_why_it_does_not_qualify() {
    let forge = FakeForge::stack_of_three();
    forge.open(4, "d", "c").open(5, "e", "d");
    forge.verdict_given(1, 100).approved(2, 200).request(3, 300).request(5, 50);
    let svc = forge.service();

    svc.refresh_inbox();

    let standings: Vec<_> = svc.inbox().rows.iter().map(|r| (r.item.id.number, r.standing)).collect();
    assert_eq!(
        standings,
        [
            (1, InboxStanding::VerdictGiven),
            (2, InboxStanding::Approved),
            (3, InboxStanding::Listed),
            (4, InboxStanding::NotRequested),
            (5, InboxStanding::Listed),
        ]
    );
}
