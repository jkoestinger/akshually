//! Seam A: new pushes to a Pull Request with an open Session, and reloading it, against a fake
//! Forge adapter whose Pull Request can be pushed to.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use akshually_core::forge::{
    ForgeAdapter, ForgeError, PullRequest, PullRequestRef, PullRequestUpdate, RemoteAnchor, RemoteComment,
    RemoteThread, Review,
};
use akshually_core::{
    Anchor, Changeset, Comment, FileChange, ForgeThread, Hunk, Party, Service, Side, ThreadRef, Verdict,
};
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
    /// The Threads on the Pull Request.
    threads: Vec<RemoteThread>,
    /// The hunks of the Forge's diff of `login.rs`, if it says.
    hunks: Option<Vec<Hunk>>,
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
            threads: Vec::new(),
            hunks: None,
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
            files: vec![FileChange {
                hunks: remote.hunks.clone(),
                ..FileChange::text("login.rs", Some("fn login() {}\n"), Some(&remote.login_rs))
            }],
        })
    }

    fn threads(&self, _: &PullRequest) -> Result<Vec<RemoteThread>, ForgeError> {
        Ok(self.0.lock().unwrap().threads.clone())
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

    fn verdicts(&self, _: &PullRequest) -> Result<Vec<Verdict>, ForgeError> {
        Ok(Verdict::ALL.to_vec())
    }

    fn publish(&self, _: &PullRequest, _: &Review) -> Result<String, ForgeError> {
        unreachable!("not published here")
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
    let update = session.pull_request_update.clone().expect("the push is offered");
    assert_eq!(update.pull_request.head_sha, "head2");
    assert_eq!(update.commits, Some(2));
    assert_eq!(session.changeset, before, "the displayed diff stays as it was");
    assert_eq!(session.pull_request().unwrap().head_sha, "head1");
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
    assert_eq!(session.pull_request().unwrap().head_sha, "head2");
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

fn remote(id: &str, anchor: RemoteAnchor, comments: &[(&str, &str)]) -> RemoteThread {
    RemoteThread {
        id: id.into(),
        anchor,
        outdated: false,
        resolved_by: None,
        diff_hunk: String::new(),
        comments: comments.iter().map(|(a, b)| RemoteComment { author: (*a).into(), body: (*b).into() }).collect(),
    }
}

fn on_file(id: &str, comments: &[(&str, &str)]) -> RemoteThread {
    remote(id, RemoteAnchor::File { path: "login.rs".into() }, comments)
}

fn said(author: &str, body: &str) -> Comment {
    Comment { author: Party::Other(author.into()), body: body.into(), round: None, suggestion: None }
}

fn draft(body: &str) -> Comment {
    Comment { author: Party::Human, body: body.into(), round: None, suggestion: None }
}

#[test]
fn reloading_takes_others_comments_and_resolutions_and_keeps_the_human_s_drafts() {
    let forge = FakeForge::new();
    forge.0.lock().unwrap().threads = vec![
        on_file("RT_1", &[("lee", "Why SSO?")]),
        on_file("RT_2", &[("lee", "Split this file?")]),
        on_file("RT_3", &[("kim", "Naming.")]),
    ];
    let svc = Service::new().with_forges(vec![Box::new(forge.clone())]);
    let id = svc.open_pull_request(URL).unwrap();
    let ids: Vec<_> = svc.session(&id).unwrap().threads.iter().map(|t| t.id.clone()).collect();
    svc.reply_in_thread(&ThreadRef::new(&id, &ids[0]), "Policy.").unwrap();
    svc.resolve_thread(&ThreadRef::new(&id, &ids[1])).unwrap();
    let new = svc.comment(&id, Anchor::lines(Side::New, "login.rs", 2..=2), "Which provider?").unwrap();
    {
        let mut remote = forge.0.lock().unwrap();
        remote.threads[0].comments.push(RemoteComment { author: "maria".into(), body: "See #12.".into() });
        remote.threads[0].resolved_by = Some("maria".into());
        remote.threads[2].resolved_by = Some("kim".into());
        remote.threads.push(on_file("RT_4", &[("kim", "Tests?")]));
    }

    svc.reload(&id).unwrap();

    let session = svc.session(&id).unwrap();
    let thread = |t: &akshually_core::ThreadId| session.threads.iter().find(|th| &th.id == t).unwrap().clone();
    let replied = thread(&ids[0]);
    assert_eq!(replied.comments, vec![said("lee", "Why SSO?"), said("maria", "See #12."), draft("Policy.")]);
    assert_eq!(replied.resolved, Some(Party::Other("maria".into())), "resolved by someone else meanwhile");
    assert_eq!(replied.forge, Some(ForgeThread { id: "RT_1".into(), resolved: true }));
    let resolved_here = thread(&ids[1]);
    assert_eq!(resolved_here.resolved, Some(Party::Human), "the human's resolve still waits for Publish");
    assert_eq!(resolved_here.forge, Some(ForgeThread { id: "RT_2".into(), resolved: false }));
    assert_eq!(thread(&ids[2]).resolved, Some(Party::Other("kim".into())));
    assert_eq!(thread(&new).comments, vec![draft("Which provider?")], "the new Draft stays");
    let added = session.threads.iter().find(|t| t.forge.as_ref().is_some_and(|f| f.id == "RT_4")).expect("RT_4");
    assert_eq!(added.comments, vec![said("kim", "Tests?")]);
    let mut all: Vec<_> = session.threads.iter().map(|t| t.id.clone()).collect();
    all.sort_by_key(|t| t.to_string());
    all.dedup();
    assert_eq!(all.len(), 5, "every Thread keeps its own id");
}

#[test]
fn a_thread_deleted_on_the_forge_goes_unless_it_holds_drafts() {
    let forge = FakeForge::new();
    forge.0.lock().unwrap().threads = vec![on_file("RT_1", &[("lee", "Hm.")]), on_file("RT_2", &[("lee", "Hm?")])];
    let svc = Service::new().with_forges(vec![Box::new(forge.clone())]);
    let id = svc.open_pull_request(URL).unwrap();
    let ids: Vec<_> = svc.session(&id).unwrap().threads.iter().map(|t| t.id.clone()).collect();
    svc.reply_in_thread(&ThreadRef::new(&id, &ids[1]), "Yes.").unwrap();
    forge.0.lock().unwrap().threads.clear();

    svc.reload(&id).unwrap();

    let session = svc.session(&id).unwrap();
    assert_eq!(session.threads.len(), 1, "the one without Drafts went");
    assert_eq!(session.threads[0].id, ids[1]);
    assert_eq!(session.threads[0].forge, None, "nowhere to reply to any more");
    assert_eq!(svc.publish_plan(&id).unwrap().discarded.len(), 1, "the reply can't be sent");
}

#[test]
fn a_draft_whose_lines_leave_the_forge_s_diff_after_a_reload_is_not_sent() {
    let forge = FakeForge::new();
    let whole = Hunk { old_start: 1, old_lines: 1, new_start: 1, new_lines: 3 };
    forge.0.lock().unwrap().hunks = Some(vec![whole]);
    let svc = Service::new().with_forges(vec![Box::new(forge.clone())]);
    let id = svc.open_pull_request(URL).unwrap();
    let draft = svc.comment(&id, Anchor::lines(Side::New, "login.rs", 2..=2), "Which provider?").unwrap();
    {
        // Only the end of the file differs from the base now.
        let mut remote = forge.0.lock().unwrap();
        remote.hunks = Some(vec![Hunk { old_start: 1, old_lines: 1, new_start: 4, new_lines: 2 }]);
    }
    forge.push("head2", "fn login() {\n    sso();\n}\n// audit\nfn audit() {}\n");

    svc.reload(&id).unwrap();

    let plan = svc.publish_plan(&id).unwrap();
    assert!(plan.kept.is_empty(), "GitHub would refuse it");
    assert_eq!(plan.discarded.iter().map(|t| t.id.clone()).collect::<Vec<_>>(), vec![draft]);
}
