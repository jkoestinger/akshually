//! Seam A: Pull Request Sessions in the core review service, with a fake Forge adapter.

use std::sync::{Arc, Mutex};

use akshually_core::forge::{
    ForgeAdapter, ForgeError, ForgeKind, PullRequest, PullRequestRef, RemoteAnchor, RemoteComment, RemoteThread, Review,
};
use akshually_core::{Anchor, Changeset, Error, FileChange, Party, ReviewRequest, Service, Side, Turn, Verdict};
use akshually_testkit::{TestDirs, TestRepo};

const URL: &str = "https://forge.test/acme/web/pull/7";

/// A Forge with one Pull Request, `acme/web#7`, counting the times it's fetched.
#[derive(Default)]
struct FakeForge {
    fetches: Arc<Mutex<u32>>,
}

fn pr_ref() -> PullRequestRef {
    PullRequestRef { host: "forge.test".into(), repo: "acme/web".into(), number: 7 }
}

impl ForgeAdapter for FakeForge {
    fn recognise(&self, url: &str) -> Option<PullRequestRef> {
        url.starts_with("https://forge.test/acme/web/pull/7").then(pr_ref)
    }

    fn pull_request(&self, pr: &PullRequestRef) -> Result<PullRequest, ForgeError> {
        *self.fetches.lock().unwrap() += 1;
        assert_eq!(pr, &pr_ref());
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

    fn changeset(&self, pr: &PullRequest) -> Result<Changeset, ForgeError> {
        assert_eq!(pr.head_sha, "headsha");
        Ok(Changeset {
            base: "basesha".into(),
            files: vec![FileChange::text("login.rs", Some("fn login() {}\n"), Some("fn login() {\n    sso();\n}\n"))],
        })
    }

    fn threads(&self, _: &PullRequest) -> Result<Vec<RemoteThread>, ForgeError> {
        Ok(vec![
            RemoteThread {
                id: "RT_1".into(),
                anchor: RemoteAnchor::Lines { path: "login.rs".into(), side: Side::New, start: 2, end: 2 },
                outdated: false,
                resolved_by: Some("maria".into()),
                diff_hunk: "@@ -1 +1,3 @@\n-fn login() {}\n+fn login() {\n+    sso();\n".into(),
                comments: vec![
                    RemoteComment { author: "lee".into(), body: "Which SSO provider?".into() },
                    RemoteComment { author: "maria".into(), body: "Okta.".into() },
                ],
            },
            RemoteThread {
                id: "RT_2".into(),
                anchor: RemoteAnchor::Lines { path: "login.rs".into(), side: Side::New, start: 9, end: 9 },
                outdated: true,
                resolved_by: None,
                diff_hunk: "@@ -1 +1,9 @@\n+    old_code();\n".into(),
                comments: vec![RemoteComment { author: "lee".into(), body: "Gone now?".into() }],
            },
            RemoteThread {
                id: "RT_3".into(),
                anchor: RemoteAnchor::File { path: "login.rs".into() },
                outdated: false,
                resolved_by: None,
                diff_hunk: String::new(),
                comments: vec![RemoteComment { author: "sam".into(), body: "Split this file?".into() }],
            },
        ])
    }

    fn verdicts(&self, _: &PullRequest) -> Result<Vec<Verdict>, ForgeError> {
        unreachable!("not published here")
    }

    fn publish(&self, _: &PullRequest, _: &Review) -> Result<String, ForgeError> {
        unreachable!("not published here")
    }
}

fn service() -> (Service, Arc<Mutex<u32>>) {
    let forge = FakeForge::default();
    let fetches = forge.fetches.clone();
    (Service::new().with_forges(vec![Box::new(forge)]), fetches)
}

#[test]
fn opening_a_pull_request_url_starts_a_session_with_its_changeset() {
    let (svc, _) = service();

    let id = svc.open_pull_request(URL).unwrap();

    let session = svc.session(&id).expect("the Session is open");
    let pr = session.pull_request().expect("a Pull Request Session").clone();
    assert_eq!(pr.id, pr_ref());
    assert_eq!(pr.title, "Add SSO login");
    assert_eq!(session.branch(), "sso");
    assert_eq!(session.changeset.base, "basesha");
    assert_eq!(session.changeset.files[0].path, "login.rs");
    assert_eq!(session.turn, Turn::Human);
}

#[test]
fn the_opened_pull_request_is_displayed_even_when_already_open() {
    let (svc, _) = service();
    let pr = svc.open_pull_request(URL).unwrap();
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n").commit_all("base");
    repo.write("a.txt", "b\n");
    let local = svc.review(ReviewRequest::new(repo.path())).unwrap();
    svc.display(local.session()).unwrap();

    svc.open_pull_request(URL).unwrap();

    assert_eq!(svc.displayed(), Some(pr.clone()));
    assert!(!svc.session(&pr).unwrap().attention);
}

#[test]
fn existing_threads_from_other_reviewers_are_shown_in_place() {
    let (svc, _) = service();

    let id = svc.open_pull_request(URL).unwrap();

    let threads = svc.session(&id).unwrap().threads;
    assert_eq!(threads.len(), 3);
    let t = &threads[0];
    assert_eq!(t.id.as_str(), "T1");
    assert_eq!(t.anchor, Anchor::lines(Side::New, "login.rs", 2..=2));
    assert_eq!(t.excerpt, "    sso();\n");
    assert_eq!(t.resolved, Some(Party::Other("maria".into())), "resolved on the Forge");
    assert!(!t.outdated);
    let comments: Vec<_> = t.comments.iter().map(|c| (c.author.clone(), c.body.as_str(), c.is_draft())).collect();
    assert_eq!(
        comments,
        vec![
            (Party::Other("lee".into()), "Which SSO provider?", false),
            (Party::Other("maria".into()), "Okta.", false)
        ]
    );
}

#[test]
fn a_thread_on_a_whole_file_is_anchored_to_the_file() {
    let (svc, _) = service();

    let id = svc.open_pull_request(URL).unwrap();

    let t = &svc.session(&id).unwrap().threads[2];
    assert_eq!(t.anchor, Anchor::File { path: "login.rs".into() });
    assert_eq!(t.comments[0].author, Party::Other("sam".into()));
}

#[test]
fn an_outdated_thread_keeps_the_code_it_was_written_against() {
    let (svc, _) = service();

    let id = svc.open_pull_request(URL).unwrap();

    let t = &svc.session(&id).unwrap().threads[1];
    assert_eq!(t.excerpt, "@@ -1 +1,9 @@\n+    old_code();\n");
    assert!(t.outdated);
}

#[test]
fn opening_the_same_pull_request_again_returns_its_session() {
    let (svc, fetches) = service();
    let first = svc.open_pull_request(URL).unwrap();

    let again = svc.open_pull_request(&format!("{URL}/files")).unwrap();

    assert_eq!(first, again);
    assert_eq!(svc.sessions().len(), 1);
    assert_eq!(*fetches.lock().unwrap(), 1, "an open Session is not fetched again");
}

#[test]
fn a_url_no_forge_recognises_is_refused() {
    let (svc, _) = service();

    let error = svc.open_pull_request("https://elsewhere.test/acme/web/pull/7").unwrap_err();

    assert!(matches!(error, Error::NotAPullRequest(_)), "{error:?}");
    assert!(svc.sessions().is_empty());
}

#[test]
fn drafts_on_a_pull_request_session_are_kept_in_the_session() {
    let (svc, _) = service();
    let id = svc.open_pull_request(URL).unwrap();

    let thread = svc.comment(&id, Anchor::lines(Side::New, "login.rs", 1..=3), "Needs a test.").unwrap();

    let session = svc.session(&id).unwrap();
    assert_eq!(thread.as_str(), "T4", "numbered after the existing Threads");
    let t = session.threads.iter().find(|t| t.id == thread).unwrap();
    assert_eq!(t.comments[0].author, Party::Human);
    assert!(t.comments[0].is_draft());
}

#[test]
fn a_forge_failure_opens_no_session() {
    struct Down;
    impl ForgeAdapter for Down {
        fn recognise(&self, _: &str) -> Option<PullRequestRef> {
            Some(pr_ref())
        }
        fn pull_request(&self, _: &PullRequestRef) -> Result<PullRequest, ForgeError> {
            Err(ForgeError::NoCredentials { host: "forge.test".into() })
        }
        fn changeset(&self, _: &PullRequest) -> Result<Changeset, ForgeError> {
            unreachable!()
        }
        fn threads(&self, _: &PullRequest) -> Result<Vec<RemoteThread>, ForgeError> {
            unreachable!()
        }
        fn verdicts(&self, _: &PullRequest) -> Result<Vec<Verdict>, ForgeError> {
            unreachable!()
        }
        fn publish(&self, _: &PullRequest, _: &Review) -> Result<String, ForgeError> {
            unreachable!()
        }
    }
    let svc = Service::new().with_forges(vec![Box::new(Down)]);

    let error = svc.open_pull_request(URL).unwrap_err();

    assert!(matches!(error, Error::Forge(ForgeError::NoCredentials { .. })), "{error:?}");
    assert!(svc.sessions().is_empty());
}

#[test]
fn checking_the_disk_leaves_pull_request_sessions_alone() {
    let (svc, _) = service();
    let id = svc.open_pull_request(URL).unwrap();
    let before = svc.session(&id).unwrap();

    svc.check_disk();

    assert_eq!(svc.session(&id).unwrap(), before);
}

#[test]
fn reloading_a_pull_request_without_a_push_changes_nothing() {
    let (svc, _) = service();
    let id = svc.open_pull_request(URL).unwrap();
    let before = svc.session(&id).unwrap();

    svc.reload(&id).unwrap();
    assert_eq!(svc.session(&id).unwrap(), before);
}

#[test]
fn a_pull_request_session_survives_a_restart() {
    let dirs = TestDirs::new();
    let fetches = Arc::new(Mutex::new(0));
    let svc = Service::open(dirs.state()).unwrap().with_forges(vec![Box::new(FakeForge { fetches: fetches.clone() })]);
    let id = svc.open_pull_request(URL).unwrap();
    svc.comment(&id, Anchor::lines(Side::New, "login.rs", 1..=3), "Needs a test.").unwrap();
    let before = svc.session(&id).unwrap();
    drop(svc);

    let restored =
        Service::open(dirs.state()).unwrap().with_forges(vec![Box::new(FakeForge { fetches: fetches.clone() })]);

    assert_eq!(restored.session(&id), Some(before));
    assert_eq!(restored.open_pull_request(URL).unwrap(), id);
    assert_eq!(*fetches.lock().unwrap(), 1, "the restored Session is not fetched again");
}

#[test]
fn a_pull_request_session_is_titled_by_its_repository_and_number() {
    let (svc, _) = service();

    let id = svc.open_pull_request(URL).unwrap();

    assert_eq!(svc.session(&id).unwrap().title(), "acme/web #7");
}

#[test]
fn a_gitlab_merge_request_takes_gitlabs_sigil() {
    let mr = PullRequest {
        id: PullRequestRef { host: "gitlab.example.com".into(), repo: "platform/infra".into(), number: 97 },
        url: "https://gitlab.example.com/platform/infra/-/merge_requests/97".into(),
        title: "Rotate database credentials".into(),
        author: "tom".into(),
        head_branch: "rotate".into(),
        head_sha: "h".into(),
        base_sha: "b".into(),
    };

    assert_eq!(mr.kind(), ForgeKind::GitLab);
    assert_eq!(mr.reference(), "platform/infra !97");
}
