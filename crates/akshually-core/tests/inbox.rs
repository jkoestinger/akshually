//! Seam A: the Inbox in the core review service, with fake Forge adapters.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use akshually_core::forge::{
    ForgeAdapter, ForgeError, ForgeKind, InboxItem, InboxRole, PullRequest, PullRequestRef, RemoteThread, Review,
};
use akshually_core::{Anchor, Changeset, FileChange, InboxRowState, ReviewRequest, Service, Side, Verdict};
use akshually_testkit::{TestDirs, TestRepo, wait_until};

/// One Forge host whose Inbox the test sets, serving each listed Pull Request.
#[derive(Clone)]
struct FakeForge {
    host: &'static str,
    inbox: Arc<Mutex<Result<Vec<InboxItem>, String>>>,
    asked: Arc<Mutex<u32>>,
}

impl FakeForge {
    fn new(host: &'static str) -> Self {
        FakeForge { host, inbox: Arc::new(Mutex::new(Ok(Vec::new()))), asked: Arc::default() }
    }

    fn lists(&self, items: Vec<InboxItem>) {
        *self.inbox.lock().unwrap() = Ok(items);
    }

    fn fails(&self) {
        *self.inbox.lock().unwrap() = Err("down".into());
    }

    fn asked(&self) -> u32 {
        *self.asked.lock().unwrap()
    }

    /// Pull Request `number` of `acme/web` on this host, updated at `updated_at`.
    fn item(&self, number: u64, role: InboxRole, updated_at: i64) -> InboxItem {
        InboxItem {
            id: PullRequestRef { host: self.host.into(), repo: "acme/web".into(), number },
            kind: ForgeKind::GitHub,
            url: format!("https://{}/acme/web/pull/{number}", self.host),
            title: format!("Change {number}"),
            author: "maria".into(),
            created_at: 1_000,
            updated_at,
            role,
        }
    }
}

impl ForgeAdapter for FakeForge {
    fn recognise(&self, url: &str) -> Option<PullRequestRef> {
        let number = url.strip_prefix(&format!("https://{}/acme/web/pull/", self.host))?;
        Some(PullRequestRef { host: self.host.into(), repo: "acme/web".into(), number: number.parse().ok()? })
    }

    fn pull_request(&self, pr: &PullRequestRef) -> Result<PullRequest, ForgeError> {
        Ok(PullRequest {
            id: pr.clone(),
            url: format!("https://{}/acme/web/pull/{}", self.host, pr.number),
            title: format!("Change {}", pr.number),
            author: "maria".into(),
            head_branch: "feature".into(),
            head_sha: "head".into(),
            base_sha: "base".into(),
        })
    }

    fn changeset(&self, _: &PullRequest) -> Result<Changeset, ForgeError> {
        Ok(Changeset { base: "base".into(), files: vec![FileChange::text("a.rs", Some("a\n"), Some("b\n"))] })
    }

    fn threads(&self, _: &PullRequest) -> Result<Vec<RemoteThread>, ForgeError> {
        Ok(Vec::new())
    }

    fn inbox(&self) -> Result<Vec<InboxItem>, ForgeError> {
        *self.asked.lock().unwrap() += 1;
        self.inbox.lock().unwrap().clone().map_err(|message| ForgeError::Http { host: self.host.into(), message })
    }

    fn verdicts(&self, _: &PullRequest) -> Result<Vec<Verdict>, ForgeError> {
        Ok(Verdict::ALL.to_vec())
    }

    fn publish(&self, pr: &PullRequest, _: &Review) -> Result<String, ForgeError> {
        Ok(format!("{}#review", pr.url))
    }
}

fn service(forges: &[&FakeForge]) -> Service {
    Service::new().with_forges(forges.iter().map(|f| Box::new((*f).clone()) as Box<dyn ForgeAdapter>).collect())
}

fn listed(svc: &Service) -> Vec<(String, u64, InboxRowState)> {
    svc.inbox().rows.into_iter().map(|r| (r.item.id.host, r.item.id.number, r.state)).collect()
}

#[test]
fn the_inbox_lists_pull_requests_from_every_host_most_recently_updated_first() {
    let (github, enterprise) = (FakeForge::new("github.com"), FakeForge::new("github.example.com"));
    github.lists(vec![github.item(1, InboxRole::Reviewer, 100), github.item(2, InboxRole::Assignee, 300)]);
    enterprise.lists(vec![enterprise.item(9, InboxRole::Reviewer, 200)]);
    let svc = service(&[&github, &enterprise]);
    assert!(svc.inbox().rows.is_empty(), "nothing before the first refresh");

    svc.refresh_inbox();

    let rows: Vec<_> = listed(&svc).into_iter().map(|(host, n, _)| (host, n)).collect();
    assert_eq!(rows, vec![("github.com".into(), 2), ("github.example.com".into(), 9), ("github.com".into(), 1)]);
    let inbox = svc.inbox();
    assert!(inbox.refreshed_at.is_some());
    assert!(inbox.errors.is_empty(), "{:?}", inbox.errors);
}

#[test]
fn a_refresh_drops_pull_requests_that_left_the_inbox() {
    let github = FakeForge::new("github.com");
    github.lists(vec![github.item(1, InboxRole::Reviewer, 100), github.item(2, InboxRole::Reviewer, 100)]);
    let svc = service(&[&github]);
    svc.refresh_inbox();

    github.lists(vec![github.item(2, InboxRole::Reviewer, 100)]);
    svc.refresh_inbox();

    assert_eq!(listed(&svc).iter().map(|r| r.1).collect::<Vec<_>>(), vec![2]);
}

#[test]
fn a_host_that_fails_keeps_its_last_rows_and_reports_why() {
    let (github, enterprise) = (FakeForge::new("github.com"), FakeForge::new("github.example.com"));
    github.lists(vec![github.item(1, InboxRole::Reviewer, 100)]);
    enterprise.lists(vec![enterprise.item(9, InboxRole::Reviewer, 200)]);
    let svc = service(&[&github, &enterprise]);
    svc.refresh_inbox();

    enterprise.fails();
    github.lists(vec![github.item(1, InboxRole::Reviewer, 100), github.item(2, InboxRole::Reviewer, 50)]);
    svc.refresh_inbox();

    let rows: Vec<_> = listed(&svc).into_iter().map(|(host, n, _)| (host, n)).collect();
    assert_eq!(rows, vec![("github.example.com".into(), 9), ("github.com".into(), 1), ("github.com".into(), 2)]);
    let errors = svc.inbox().errors;
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("github.example.com"), "{errors:?}");
}

#[test]
fn a_pull_request_never_opened_is_new() {
    let github = FakeForge::new("github.com");
    github.lists(vec![github.item(1, InboxRole::Reviewer, 100)]);
    let svc = service(&[&github]);

    svc.refresh_inbox();

    assert_eq!(listed(&svc)[0].2, InboxRowState::New);
}

#[test]
fn an_open_session_shows_on_its_row_with_its_draft_count() {
    let github = FakeForge::new("github.com");
    github.lists(vec![github.item(1, InboxRole::Reviewer, 100)]);
    let svc = service(&[&github]);
    svc.refresh_inbox();

    let id = svc.open_pull_request(&svc.inbox().rows[0].item.url).unwrap();
    svc.comment(&id, Anchor::lines(Side::New, "a.rs", 1..=1), "Why?").unwrap();
    svc.comment(&id, Anchor::file("a.rs"), "Split it.").unwrap();

    assert_eq!(listed(&svc)[0].2, InboxRowState::SessionOpen { session: id, drafts: 2 });
}

#[test]
fn a_published_session_stays_on_its_row_until_closed_and_its_own_review_is_no_update() {
    let github = FakeForge::new("github.com");
    github.lists(vec![github.item(1, InboxRole::Assignee, 100)]);
    let svc = service(&[&github]);
    svc.refresh_inbox();
    let id = svc.open_pull_request(&svc.inbox().rows[0].item.url).unwrap();
    svc.comment(&id, Anchor::file("a.rs"), "Split it.").unwrap();

    svc.publish(&id, Verdict::RequestChanges, "").unwrap();
    assert_eq!(listed(&svc)[0].2, InboxRowState::SessionOpen { session: id.clone(), drafts: 0 });

    svc.close(&id).unwrap();
    // The review just Published updated the Pull Request on the Forge.
    github.lists(vec![github.item(1, InboxRole::Assignee, 200)]);
    svc.refresh_inbox();
    assert_eq!(listed(&svc)[0].2, InboxRowState::Seen);
}

#[test]
fn a_pull_request_looked_at_is_seen_until_it_is_updated() {
    let github = FakeForge::new("github.com");
    github.lists(vec![github.item(1, InboxRole::Reviewer, 100)]);
    let svc = service(&[&github]);
    svc.refresh_inbox();
    let id = svc.open_pull_request(&svc.inbox().rows[0].item.url).unwrap();

    svc.close(&id).unwrap();
    assert!(svc.session(&id).is_none(), "closing a Pull Request Session ends it");
    assert_eq!(listed(&svc)[0].2, InboxRowState::Seen);

    github.lists(vec![github.item(1, InboxRole::Reviewer, 150)]);
    svc.refresh_inbox();
    assert_eq!(listed(&svc)[0].2, InboxRowState::Updated);
}

#[test]
fn a_new_review_request_marks_the_inbox() {
    let github = FakeForge::new("github.com");
    let svc = service(&[&github]);
    svc.refresh_inbox();
    assert!(!svc.inbox().attention);

    github.lists(vec![github.item(1, InboxRole::Reviewer, 100)]);
    svc.refresh_inbox();

    assert!(svc.inbox().attention);
}

#[test]
fn displaying_the_inbox_clears_its_attention() {
    let github = FakeForge::new("github.com");
    let svc = service(&[&github]);
    github.lists(vec![github.item(1, InboxRole::Reviewer, 100)]);
    svc.refresh_inbox();

    svc.display_inbox();

    assert!(!svc.inbox().attention);
    assert!(svc.inbox_displayed());
    assert_eq!(svc.displayed(), None);
}

#[test]
fn only_a_new_review_request_marks_the_inbox() {
    let github = FakeForge::new("github.com");
    github.lists(vec![github.item(1, InboxRole::Reviewer, 100)]);
    let svc = service(&[&github]);
    svc.refresh_inbox();
    svc.display_inbox();

    // The same request updated, and a new assignment.
    github.lists(vec![github.item(1, InboxRole::Reviewer, 200), github.item(2, InboxRole::Assignee, 200)]);
    svc.refresh_inbox();
    svc.open_pull_request("https://github.com/acme/web/pull/1").unwrap();
    svc.refresh_inbox();

    assert!(!svc.inbox().attention);
}

#[test]
fn a_review_request_arriving_while_the_inbox_is_displayed_does_not_mark_it() {
    let github = FakeForge::new("github.com");
    let svc = service(&[&github]);
    svc.display_inbox();

    github.lists(vec![github.item(1, InboxRole::Reviewer, 100)]);
    svc.refresh_inbox();

    assert!(!svc.inbox().attention);
}

#[test]
fn a_session_needing_the_human_while_the_inbox_is_displayed_gets_attention() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n").commit_all("base");
    repo.write("a.txt", "b\n");
    let svc = Service::new();
    svc.display_inbox();

    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();

    assert_eq!(svc.displayed(), None, "the Inbox keeps the view");
    assert!(svc.session(pending.session()).unwrap().attention);
}

#[test]
fn opening_a_session_leaves_the_inbox() {
    let github = FakeForge::new("github.com");
    let svc = service(&[&github]);
    svc.display_inbox();

    let id = svc.open_pull_request("https://github.com/acme/web/pull/3").unwrap();

    assert!(!svc.inbox_displayed());
    assert_eq!(svc.displayed(), Some(id));
}

#[test]
fn inbox_subscribers_hear_of_each_refresh() {
    let github = FakeForge::new("github.com");
    github.lists(vec![github.item(1, InboxRole::Reviewer, 100)]);
    let svc = service(&[&github]);
    let heard = Arc::new(Mutex::new(Vec::new()));
    let sink = heard.clone();
    svc.subscribe_inbox(move |inbox| sink.lock().unwrap().push(inbox.rows.len()));

    svc.refresh_inbox();

    assert_eq!(*heard.lock().unwrap(), vec![1]);
}

#[test]
fn the_inbox_refreshes_on_a_timer() {
    let github = FakeForge::new("github.com");
    let svc = Arc::new(service(&[&github]));

    svc.watch_inbox(Duration::from_millis(20));

    wait_until(Duration::from_secs(10), "a refresh at once", || (github.asked() >= 1).then_some(()));
    github.lists(vec![github.item(1, InboxRole::Reviewer, 100)]);
    wait_until(Duration::from_secs(10), "a later refresh", || (!svc.inbox().rows.is_empty()).then_some(()));
}

#[test]
fn the_inbox_and_what_the_human_saw_survive_a_restart() {
    let dirs = TestDirs::new();
    let github = FakeForge::new("github.com");
    github.lists(vec![github.item(1, InboxRole::Reviewer, 100), github.item(2, InboxRole::Reviewer, 100)]);
    let svc = Service::open(dirs.state()).unwrap().with_forges(vec![Box::new(github.clone())]);
    svc.refresh_inbox();
    let id = svc.open_pull_request("https://github.com/acme/web/pull/1").unwrap();
    svc.abandon(&id).unwrap();
    svc.display_inbox();
    let before = svc.inbox();
    drop(svc);

    let restored = Service::open(dirs.state()).unwrap().with_forges(vec![Box::new(github.clone())]);

    assert_eq!(restored.inbox(), before);
    restored.refresh_inbox();
    let states: Vec<_> = listed(&restored).into_iter().map(|(_, n, state)| (n, state)).collect();
    assert_eq!(states, vec![(1, InboxRowState::Seen), (2, InboxRowState::New)]);
    assert!(!restored.inbox().attention, "the requests listed before the restart aren't new");
}

#[test]
fn inbox_attention_survives_a_restart() {
    let dirs = TestDirs::new();
    let github = FakeForge::new("github.com");
    github.lists(vec![github.item(1, InboxRole::Reviewer, 100)]);
    let svc = Service::open(dirs.state()).unwrap().with_forges(vec![Box::new(github.clone())]);
    svc.refresh_inbox();
    drop(svc);

    let restored = Service::open(dirs.state()).unwrap();

    assert!(restored.inbox().attention);
}

#[test]
fn a_pull_request_listed_as_assignee_and_reviewer_is_a_review_request() {
    let github = FakeForge::new("github.com");
    github.lists(vec![github.item(1, InboxRole::Assignee, 100), github.item(1, InboxRole::Reviewer, 100)]);
    let svc = service(&[&github]);

    svc.refresh_inbox();

    let inbox = svc.inbox();
    assert_eq!(inbox.rows.iter().map(|r| r.item.role).collect::<Vec<_>>(), vec![InboxRole::Reviewer]);
    assert!(inbox.attention, "a new review request");
}

#[test]
fn inbox_subscribers_hear_when_a_session_changes_a_rows_state() {
    let github = FakeForge::new("github.com");
    github.lists(vec![github.item(1, InboxRole::Reviewer, 100)]);
    let svc = service(&[&github]);
    svc.refresh_inbox();
    let id = svc.open_pull_request("https://github.com/acme/web/pull/1").unwrap();
    let heard = Arc::new(Mutex::new(Vec::new()));
    let sink = heard.clone();
    svc.subscribe_inbox(move |inbox| sink.lock().unwrap().push(inbox.rows[0].state.clone()));

    svc.comment(&id, Anchor::file("a.rs"), "Why b?").unwrap();

    let last = heard.lock().unwrap().last().cloned();
    assert_eq!(last, Some(InboxRowState::SessionOpen { session: id, drafts: 1 }));
}
