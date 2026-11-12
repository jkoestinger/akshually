//! Seam A: Publish from a Pull Request Session, with a fake Forge adapter that records the
//! review it is handed.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use akshually_core::forge::{
    ForgeAdapter, ForgeError, PullRequest, PullRequestRef, RemoteAnchor, RemoteComment, RemoteThread, Review,
    ReviewComment,
};
use akshually_core::{Anchor, Changeset, Error, FileChange, NewComment, Outcome, Service, Side, Verdict};

const URL: &str = "https://forge.test/acme/web/pull/7";

/// Publish signals it started, then waits for a go.
type Hold = (Mutex<Sender<()>>, Mutex<Receiver<()>>);

/// A Forge with one Pull Request, `acme/web#7`, by `author`, that records each review published.
struct FakeForge {
    author: String,
    published: Arc<Mutex<Vec<Review>>>,
    /// Publish fails, as when the Forge refuses the review.
    refuse: bool,
    /// Publish says it started, then waits to be let through.
    hold: Option<Hold>,
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
            author: self.author.clone(),
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
        Ok(vec![RemoteThread {
            id: "RT_1".into(),
            anchor: RemoteAnchor::File { path: "README.md".into() },
            outdated: false,
            resolved_by: None,
            diff_hunk: String::new(),
            comments: vec![RemoteComment { author: "lee".into(), body: "Mention Okta?".into() }],
        }])
    }

    fn verdicts(&self, pr: &PullRequest) -> Result<Vec<Verdict>, ForgeError> {
        Ok(if pr.author == "me" { vec![Verdict::Feedback] } else { Verdict::ALL.to_vec() })
    }

    fn publish(&self, _: &PullRequest, review: &Review) -> Result<String, ForgeError> {
        if let Some((started, release)) = &self.hold {
            started.lock().unwrap().send(()).unwrap();
            release.lock().unwrap().recv().unwrap();
        }
        if self.refuse {
            let message = "Validation Failed".into();
            return Err(ForgeError::Api { host: "forge.test".into(), status: 422, message });
        }
        self.published.lock().unwrap().push(review.clone());
        Ok(format!("{URL}#pullrequestreview-1"))
    }
}

fn service_for(author: &str) -> (Service, Arc<Mutex<Vec<Review>>>) {
    let published = Arc::new(Mutex::new(Vec::new()));
    let forge = FakeForge { author: author.into(), published: published.clone(), refuse: false, hold: None };
    (Service::new().with_forges(vec![Box::new(forge)]), published)
}

#[test]
fn publish_sends_one_review_with_every_kept_draft_the_summary_and_the_verdict() {
    let (svc, published) = service_for("maria");
    let id = svc.open_pull_request(URL).unwrap();
    svc.comment(
        &id,
        Anchor::lines(Side::New, "login.rs", 2..=2),
        NewComment::new("Name the provider.").suggest("    okta();\n"),
    )
    .unwrap();
    svc.comment(&id, Anchor::lines(Side::Old, "login.rs", 1..=1), "Why was this removed?").unwrap();
    svc.comment(&id, Anchor::file("README.md"), "Link the docs.").unwrap();

    svc.publish(&id, Verdict::RequestChanges, "A few things first.").unwrap();

    let published = published.lock().unwrap();
    assert_eq!(published.len(), 1, "exactly one review");
    assert_eq!(
        published[0],
        Review {
            verdict: Verdict::RequestChanges,
            body: "A few things first.".into(),
            comments: vec![
                ReviewComment {
                    anchor: RemoteAnchor::Lines { path: "login.rs".into(), side: Side::New, start: 2, end: 2 },
                    body: "Name the provider.".into(),
                    suggestion: Some("    okta();\n".into()),
                },
                ReviewComment {
                    anchor: RemoteAnchor::Lines { path: "login.rs".into(), side: Side::Old, start: 1, end: 1 },
                    body: "Why was this removed?".into(),
                    suggestion: None,
                },
                ReviewComment {
                    anchor: RemoteAnchor::File { path: "README.md".into() },
                    body: "Link the docs.".into(),
                    suggestion: None,
                },
            ],
        }
    );
}

#[test]
fn drafts_on_the_whole_review_join_the_summary_in_the_review_body() {
    let (svc, published) = service_for("maria");
    let id = svc.open_pull_request(URL).unwrap();
    svc.comment(&id, Anchor::Review, "Nice and small.").unwrap();

    svc.publish(&id, Verdict::Approve, "LGTM.\n").unwrap();

    let published = published.lock().unwrap();
    assert_eq!(published[0].body, "LGTM.\n\nNice and small.");
    assert!(published[0].comments.is_empty());
}

#[test]
fn on_your_own_pull_request_only_feedback_is_offered() {
    let (svc, _) = service_for("me");
    let id = svc.open_pull_request(URL).unwrap();

    assert_eq!(svc.publish_plan(&id).unwrap().verdicts, vec![Verdict::Feedback]);
}

#[test]
fn after_publish_the_session_has_its_verdict_and_can_be_closed() {
    let (svc, _) = service_for("maria");
    let id = svc.open_pull_request(URL).unwrap();
    svc.comment(&id, Anchor::file("README.md"), "Link the docs.").unwrap();

    let url = svc.publish(&id, Verdict::Approve, "").unwrap();

    let session = svc.session(&id).unwrap();
    assert_eq!(url, format!("{URL}#pullrequestreview-1"));
    assert_eq!(session.ended, Some(Outcome::Published { verdict: Verdict::Approve, url }));
    assert!(session.threads.iter().flat_map(|t| &t.comments).all(|c| !c.is_draft()), "nothing left to publish");
    assert!(matches!(svc.publish(&id, Verdict::Approve, ""), Err(Error::Ended)));

    svc.close(&id).unwrap();

    assert!(svc.sessions().is_empty());
}

#[test]
fn when_the_forge_refuses_the_review_the_session_and_its_drafts_stay_as_they_were() {
    let forge = FakeForge { author: "maria".into(), published: Arc::default(), refuse: true, hold: None };
    let svc = Service::new().with_forges(vec![Box::new(forge)]);
    let id = svc.open_pull_request(URL).unwrap();
    svc.comment(&id, Anchor::file("README.md"), "Link the docs.").unwrap();
    let before = svc.session(&id).unwrap();

    let error = svc.publish(&id, Verdict::Approve, "").unwrap_err();

    assert!(matches!(error, Error::Forge(ForgeError::Api { status: 422, .. })), "{error:?}");
    assert_eq!(svc.session(&id).unwrap(), before);
}

#[test]
fn a_session_being_published_is_not_published_twice() {
    let (started_tx, started) = channel();
    let (release, release_rx) = channel();
    let published = Arc::new(Mutex::new(Vec::new()));
    let hold = Some((Mutex::new(started_tx), Mutex::new(release_rx)));
    let forge = FakeForge { author: "maria".into(), published: published.clone(), refuse: false, hold };
    let svc = Arc::new(Service::new().with_forges(vec![Box::new(forge)]));
    let id = svc.open_pull_request(URL).unwrap();
    svc.comment(&id, Anchor::file("README.md"), "Link the docs.").unwrap();
    let first = std::thread::spawn({
        let (svc, id) = (svc.clone(), id.clone());
        move || svc.publish(&id, Verdict::Approve, "")
    });
    started.recv().unwrap();

    let second = svc.publish(&id, Verdict::Approve, "");

    release.send(()).unwrap();
    first.join().unwrap().unwrap();
    assert!(matches!(second, Err(Error::Publishing)), "{second:?}");
    assert_eq!(published.lock().unwrap().len(), 1);
}

#[test]
fn feedback_with_nothing_to_say_is_refused() {
    let (svc, published) = service_for("maria");
    let id = svc.open_pull_request(URL).unwrap();

    let error = svc.publish(&id, Verdict::Feedback, "  ").unwrap_err();

    assert!(matches!(error, Error::EmptyReview), "{error:?}");
    assert!(published.lock().unwrap().is_empty());
}

#[test]
fn the_plan_lists_what_will_be_sent_and_what_will_not() {
    let (svc, published) = service_for("maria");
    let id = svc.open_pull_request(URL).unwrap();
    let new = svc.comment(&id, Anchor::lines(Side::New, "login.rs", 1..=3), "Needs a test.").unwrap();
    let existing = svc.session(&id).unwrap().threads[0].id.clone();
    svc.reply_in_thread(&id, &existing, "Agreed.").unwrap();

    let plan = svc.publish_plan(&id).unwrap();

    let listed = |threads: &[akshually_core::Thread]| {
        threads.iter().map(|t| (t.id.clone(), t.comments.iter().map(|c| c.body.clone()).collect())).collect()
    };
    let kept: Vec<(_, Vec<String>)> = listed(&plan.kept);
    let discarded: Vec<(_, Vec<String>)> = listed(&plan.discarded);
    assert_eq!(kept, vec![(new, vec!["Needs a test.".to_owned()])]);
    assert_eq!(discarded, vec![(existing, vec!["Agreed.".to_owned()])], "replies go out from #26 on");
    assert_eq!(plan.verdicts, Verdict::ALL.to_vec());

    svc.publish(&id, Verdict::Feedback, "").unwrap();
    assert_eq!(published.lock().unwrap()[0].comments.len(), 1, "only what was kept is sent");
}

#[test]
fn a_verdict_the_forge_would_reject_is_refused_and_nothing_is_sent() {
    let (svc, published) = service_for("me");
    let id = svc.open_pull_request(URL).unwrap();
    svc.comment(&id, Anchor::file("README.md"), "Link the docs.").unwrap();

    let error = svc.publish(&id, Verdict::Approve, "").unwrap_err();

    assert!(matches!(error, Error::VerdictNotOffered(Verdict::Approve)), "{error:?}");
    assert!(published.lock().unwrap().is_empty());
    let session = svc.session(&id).unwrap();
    assert_eq!(session.ended, None);
    assert!(session.threads[1].comments[0].is_draft(), "the Draft stays");
}
