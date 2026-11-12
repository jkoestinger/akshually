//! Seam A: new pushes to a Pull Request with an open Session, and reloading it, against a fake
//! Forge adapter whose Pull Request can be pushed to.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use akshually_core::forge::{ForgeAdapter, ForgeError, PullRequest, PullRequestRef, PullRequestUpdate, RemoteThread};
use akshually_core::{Anchor, Changeset, FileChange, Service, Side};
use akshually_testkit::{TestDirs, wait_until};

const URL: &str = "https://forge.test/acme/web/pull/7";

fn pr_ref() -> PullRequestRef {
    PullRequestRef { host: "forge.test".into(), repo: "acme/web".into(), number: 7 }
}

/// The Pull Request as the Forge has it now.
struct Remote {
    head_sha: String,
    login_rs: String,
    /// Commits since each earlier head, as the Forge would count them.
    commits: Option<u32>,
    /// Calls to `update` that fail, as when the Forge is down.
    down: bool,
}

#[derive(Clone)]
struct FakeForge(Arc<Mutex<Remote>>);

impl FakeForge {
    fn new() -> Self {
        FakeForge(Arc::new(Mutex::new(Remote {
            head_sha: "head1".into(),
            login_rs: "fn login() {\n    sso();\n}\n".into(),
            commits: Some(2),
            down: false,
        })))
    }

    /// Someone pushes: the head moves and `login.rs` reads `text`.
    fn push(&self, head: &str, text: &str) {
        let mut remote = self.0.lock().unwrap();
        remote.head_sha = head.into();
        remote.login_rs = text.into();
    }

    fn current(&self) -> PullRequest {
        let remote = self.0.lock().unwrap();
        PullRequest {
            id: pr_ref(),
            url: URL.into(),
            title: "Add SSO login".into(),
            author: "maria".into(),
            head_branch: "sso".into(),
            head_sha: remote.head_sha.clone(),
            base_sha: "basesha".into(),
        }
    }
}

impl ForgeAdapter for FakeForge {
    fn recognise(&self, url: &str) -> Option<PullRequestRef> {
        url.starts_with(URL).then(pr_ref)
    }

    fn pull_request(&self, _: &PullRequestRef) -> Result<PullRequest, ForgeError> {
        Ok(self.current())
    }

    fn changeset(&self, pr: &PullRequest) -> Result<Changeset, ForgeError> {
        let remote = self.0.lock().unwrap();
        assert_eq!(pr.head_sha, remote.head_sha, "the Changeset at the head asked for");
        Ok(Changeset {
            base: "basesha".into(),
            files: vec![FileChange::text("login.rs", Some("fn login() {}\n"), Some(&remote.login_rs))],
        })
    }

    fn threads(&self, _: &PullRequest) -> Result<Vec<RemoteThread>, ForgeError> {
        Ok(Vec::new())
    }

    fn update(&self, pr: &PullRequest) -> Result<Option<PullRequestUpdate>, ForgeError> {
        let commits = {
            let remote = self.0.lock().unwrap();
            if remote.down {
                return Err(ForgeError::Http { host: "forge.test".into(), message: "down".into() });
            }
            remote.commits
        };
        let now = self.current();
        Ok((now.head_sha != pr.head_sha).then_some(PullRequestUpdate { pull_request: now, commits }))
    }
}

fn opened() -> (Service, FakeForge, akshually_core::SessionId) {
    let forge = FakeForge::new();
    let svc = Service::new().with_forges(vec![Box::new(forge.clone())]);
    let id = svc.open_pull_request(URL).unwrap();
    (svc, forge, id)
}

#[test]
fn a_push_is_offered_as_an_update_without_changing_the_diff() {
    let (svc, forge, id) = opened();
    let before = svc.session(&id).unwrap().changeset;

    forge.push("head2", "fn login() {\n    sso();\n    audit();\n}\n");
    svc.check_pull_requests();

    let session = svc.session(&id).unwrap();
    let update = session.pull_request_update.expect("the push is offered");
    assert_eq!(update.pull_request.head_sha, "head2");
    assert_eq!(update.commits, Some(2));
    assert_eq!(session.changeset, before, "the displayed diff stays as it was");
    assert_eq!(session.pull_request.unwrap().head_sha, "head1");
}

#[test]
fn reloading_takes_the_push_and_re_anchors_drafts_and_threads() {
    let (svc, forge, id) = opened();
    let moved = svc.comment(&id, Anchor::lines(Side::New, "login.rs", 2..=2), "Which provider?").unwrap();
    let gone = svc.comment(&id, Anchor::lines(Side::New, "login.rs", 3..=3), "Brace style.").unwrap();
    svc.mark_viewed(&id, "login.rs", true).unwrap();
    let pushed = "fn login() -> Result<()> {\n    audit();\n    sso();\n    Ok(())\n";
    forge.push("head2", pushed);
    svc.check_pull_requests();

    svc.reload(&id).unwrap();

    let session = svc.session(&id).unwrap();
    assert_eq!(session.pull_request.as_ref().unwrap().head_sha, "head2");
    assert_eq!(session.pull_request_update, None, "nothing left to reload");
    assert_eq!(session.changeset.files[0].new.as_deref(), Some(pushed));
    let thread = |t| session.threads.iter().find(|th| th.id == t).unwrap();
    assert_eq!(thread(moved.clone()).anchor, Anchor::lines(Side::New, "login.rs", 3..=3), "followed its code");
    assert!(!thread(moved).outdated);
    assert!(thread(gone).outdated, "its code is gone");
    assert!(session.viewed.is_empty(), "the changed file needs viewing again");
}

#[test]
fn a_forge_that_cannot_be_reached_keeps_the_offered_update() {
    let (svc, forge, id) = opened();
    forge.push("head2", "fn login() {}\n");
    svc.check_pull_requests();
    let offered = svc.session(&id).unwrap();

    forge.0.lock().unwrap().down = true;
    svc.check_pull_requests();

    assert_eq!(svc.session(&id).unwrap(), offered);
}

#[test]
fn a_restored_pull_request_session_still_offers_its_update() {
    let dirs = TestDirs::new();
    let forge = FakeForge::new();
    let svc = Service::open(dirs.state()).unwrap().with_forges(vec![Box::new(forge.clone())]);
    let id = svc.open_pull_request(URL).unwrap();
    forge.push("head2", "fn login() {}\n");
    svc.check_pull_requests();
    drop(svc);

    let restored = Service::open(dirs.state()).unwrap().with_forges(vec![Box::new(forge.clone())]);

    let update = restored.session(&id).unwrap().pull_request_update.expect("still offered");
    assert_eq!(update.pull_request.head_sha, "head2");
    restored.reload(&id).unwrap();
    assert_eq!(restored.session(&id).unwrap().changeset.files[0].new.as_deref(), Some("fn login() {}\n"));
}

#[test]
fn pull_requests_are_watched_for_pushes() {
    let forge = FakeForge::new();
    let svc = Arc::new(Service::new().with_forges(vec![Box::new(forge.clone())]));
    let id = svc.open_pull_request(URL).unwrap();
    svc.watch_pull_requests(Duration::from_millis(20));

    forge.push("head2", "fn login() {}\n");

    wait_until(Duration::from_secs(10), "the push to be offered", || {
        svc.session(&id).unwrap().pull_request_update.filter(|u| u.pull_request.head_sha == "head2")
    });
}
