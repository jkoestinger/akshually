//! Seam A: Publish from a Pull Request Session, with a fake Forge adapter that records the
//! review it is handed.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use akshually_core::forge::{
    ForgeAdapter, ForgeError, PullRequest, PullRequestRef, RemoteAnchor, RemoteComment, RemoteThread, Review,
    ReviewComment, ReviewReply,
};
use akshually_core::{
    Anchor, Changeset, Error, FileChange, ForgeThread, Hunk, NewComment, Outcome, Service, Side, ThreadRef, Verdict,
};

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
    /// The Threads already on the Pull Request.
    remote: Vec<RemoteThread>,
}

/// The one Thread on the Pull Request unless a test says otherwise: `RT_1`, on README.md.
fn readme_thread() -> RemoteThread {
    RemoteThread {
        id: "RT_1".into(),
        anchor: RemoteAnchor::File { path: "README.md".into() },
        outdated: false,
        resolved_by: None,
        diff_hunk: String::new(),
        comments: vec![RemoteComment { author: "lee".into(), body: "Mention Okta?".into() }],
    }
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
                config_rs(),
            ],
        })
    }

    fn threads(&self, _: &PullRequest) -> Result<Vec<RemoteThread>, ForgeError> {
        Ok(self.remote.clone())
    }

    /// The human is `me`.
    fn viewer(&self) -> Result<String, ForgeError> {
        Ok("me".into())
    }

    fn verdicts(&self, _: &PullRequest, own: bool) -> Result<Vec<Verdict>, ForgeError> {
        Ok(if own { vec![Verdict::Feedback] } else { Verdict::ALL.to_vec() })
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
    let forge = FakeForge {
        author: author.into(),
        published: published.clone(),
        refuse: false,
        hold: None,
        remote: vec![readme_thread()],
    };
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
            replies: Vec::new(),
            resolve: Vec::new(),
            reopen: Vec::new(),
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
fn the_plan_says_whether_the_human_wrote_the_pull_request_whatever_the_case_of_their_login() {
    let (yours, _) = service_for("Me");
    let (theirs, _) = service_for("maria");
    let (your_id, their_id) = (yours.open_pull_request(URL).unwrap(), theirs.open_pull_request(URL).unwrap());

    let (own, other) = (yours.publish_plan(&your_id).unwrap(), theirs.publish_plan(&their_id).unwrap());

    assert!(own.own, "the human is `me`, who wrote it");
    assert!(!other.own, "maria wrote it");
}

#[test]
fn the_plan_starts_from_the_only_verdict_offered_and_leaves_a_choice_to_the_human() {
    let (yours, _) = service_for("me");
    let (theirs, _) = service_for("maria");
    let (your_id, their_id) = (yours.open_pull_request(URL).unwrap(), theirs.open_pull_request(URL).unwrap());

    let (own, other) = (yours.publish_plan(&your_id).unwrap(), theirs.publish_plan(&their_id).unwrap());

    assert_eq!(own.default_verdict(), Some(Verdict::Feedback), "only feedback is offered");
    assert_eq!(other.default_verdict(), None, "every Verdict is offered");
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
    assert!(
        matches!(svc.publish(&id, Verdict::Approve, ""), Err(Error::Ended)),
        "an ended Session was Published again"
    );

    svc.close(&id).unwrap();

    assert!(svc.sessions().is_empty());
}

#[test]
fn when_the_forge_refuses_the_review_the_session_and_its_drafts_stay_as_they_were() {
    let forge = FakeForge {
        author: "maria".into(),
        published: Arc::default(),
        refuse: true,
        hold: None,
        remote: vec![readme_thread()],
    };
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
    let forge = FakeForge {
        author: "maria".into(),
        published: published.clone(),
        refuse: false,
        hold,
        remote: vec![readme_thread()],
    };
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
fn the_plan_lists_new_threads_and_replies_on_existing_ones_and_publish_sends_them() {
    let (svc, published) = service_for("maria");
    let id = svc.open_pull_request(URL).unwrap();
    let new = svc.comment(&id, Anchor::lines(Side::New, "login.rs", 1..=3), "Needs a test.").unwrap();
    let existing = svc.session(&id).unwrap().threads[0].id.clone();
    svc.reply_in_thread(&ThreadRef::new(&id, &existing), "Agreed.").unwrap();
    svc.reply_in_thread(&ThreadRef::new(&id, &existing), "Okta and Entra.").unwrap();

    let plan = svc.publish_plan(&id).unwrap();

    let listed = |threads: &[akshually_core::Thread]| {
        threads.iter().map(|t| (t.id.clone(), t.comments.iter().map(|c| c.body.clone()).collect())).collect()
    };
    let kept: Vec<(_, Vec<String>)> = listed(&plan.kept);
    let expected = vec![
        (existing.clone(), vec!["Agreed.".to_owned(), "Okta and Entra.".to_owned()]),
        (new, vec!["Needs a test.".to_owned()]),
    ];
    assert_eq!(kept, expected);
    assert!(plan.discarded.is_empty());
    assert_eq!(plan.verdicts, Verdict::ALL.to_vec());

    svc.publish(&id, Verdict::Feedback, "").unwrap();

    let published = published.lock().unwrap();
    assert_eq!(published[0].comments.len(), 1, "one new Thread");
    assert_eq!(
        published[0].replies,
        vec![ReviewReply { thread: "RT_1".into(), body: "Agreed.\n\nOkta and Entra.".into() }],
        "one reply in the existing Thread, by its Forge id"
    );
    let session = svc.session(&id).unwrap();
    let thread = session.threads.iter().find(|t| t.id == existing).unwrap();
    assert!(thread.comments.iter().all(|c| !c.is_draft()), "the replies went out");
}

#[test]
fn a_reply_alone_is_enough_for_feedback() {
    let (svc, published) = service_for("maria");
    let id = svc.open_pull_request(URL).unwrap();
    let existing = svc.session(&id).unwrap().threads[0].id.clone();
    svc.reply_in_thread(&ThreadRef::new(&id, &existing), "Done in the next PR.").unwrap();

    svc.publish(&id, Verdict::Feedback, "").unwrap();

    assert_eq!(published.lock().unwrap()[0].replies.len(), 1);
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

#[test]
fn existing_threads_keep_their_forge_ids() {
    let (svc, _) = service_for("maria");
    let id = svc.open_pull_request(URL).unwrap();

    let session = svc.session(&id).unwrap();

    assert_eq!(session.threads[0].forge, Some(ForgeThread { id: "RT_1".into(), resolved: false }));
}

/// `RT_2`, on login.rs, which `lee` resolved.
fn resolved_thread() -> RemoteThread {
    RemoteThread {
        id: "RT_2".into(),
        anchor: RemoteAnchor::Lines { path: "login.rs".into(), side: Side::New, start: 2, end: 2 },
        resolved_by: Some("lee".into()),
        comments: vec![RemoteComment { author: "lee".into(), body: "Typo.".into() }],
        ..readme_thread()
    }
}

fn service_with(remote: Vec<RemoteThread>) -> (Service, Arc<Mutex<Vec<Review>>>) {
    let published = Arc::new(Mutex::new(Vec::new()));
    let forge = FakeForge { author: "maria".into(), published: published.clone(), refuse: false, hold: None, remote };
    (Service::new().with_forges(vec![Box::new(forge)]), published)
}

#[test]
fn resolving_and_reopening_existing_threads_is_listed_and_published_with_the_review() {
    let (svc, published) = service_with(vec![readme_thread(), resolved_thread()]);
    let id = svc.open_pull_request(URL).unwrap();
    let threads = svc.session(&id).unwrap().threads;
    let (open, resolved) = (threads[0].id.clone(), threads[1].id.clone());
    svc.resolve_thread(&ThreadRef::new(&id, &open)).unwrap();
    svc.reopen_thread(&ThreadRef::new(&id, &resolved)).unwrap();

    let plan = svc.publish_plan(&id).unwrap();

    let ids = |threads: &[akshually_core::Thread]| threads.iter().map(|t| t.id.clone()).collect::<Vec<_>>();
    assert_eq!(ids(&plan.resolve), vec![open.clone()]);
    assert_eq!(ids(&plan.reopen), vec![resolved.clone()]);

    svc.publish(&id, Verdict::Approve, "").unwrap();

    let review = published.lock().unwrap()[0].clone();
    assert_eq!((review.resolve, review.reopen), (vec!["RT_1".to_owned()], vec!["RT_2".to_owned()]));
    let threads = svc.session(&id).unwrap().threads;
    assert_eq!(threads[0].forge, Some(ForgeThread { id: "RT_1".into(), resolved: true }), "resolved there now");
    assert_eq!(threads[1].forge, Some(ForgeThread { id: "RT_2".into(), resolved: false }), "reopened there now");
}

#[test]
fn a_thread_resolved_and_reopened_again_before_publish_is_left_alone() {
    let (svc, published) = service_with(vec![readme_thread()]);
    let id = svc.open_pull_request(URL).unwrap();
    let thread = svc.session(&id).unwrap().threads[0].id.clone();
    svc.resolve_thread(&ThreadRef::new(&id, &thread)).unwrap();
    svc.reopen_thread(&ThreadRef::new(&id, &thread)).unwrap();

    let plan = svc.publish_plan(&id).unwrap();
    svc.publish(&id, Verdict::Approve, "").unwrap();

    assert!(plan.resolve.is_empty() && plan.reopen.is_empty());
    let review = published.lock().unwrap()[0].clone();
    assert!(review.resolve.is_empty() && review.reopen.is_empty());
}

/// 20 lines, of which the Forge's diff shows two hunks: new lines 3-9 (old 3-8) and 15-20
/// (old 14-19).
fn config_rs() -> FileChange {
    let old: String = (1..=19).map(|n| format!("old {n}\n")).collect();
    let new: String = (1..=20).map(|n| format!("line {n}\n")).collect();
    FileChange {
        hunks: Some(vec![
            Hunk { old_start: 3, old_lines: 6, new_start: 3, new_lines: 7 },
            Hunk { old_start: 14, old_lines: 6, new_start: 15, new_lines: 6 },
        ]),
        ..FileChange::text("config.rs", Some(&old), Some(&new))
    }
}

#[test]
fn line_comments_outside_the_forge_s_diff_are_refused_and_the_file_is_offered_instead() {
    let (svc, _) = service_for("maria");
    let id = svc.open_pull_request(URL).unwrap();

    let refused = |anchor| svc.comment(&id, anchor, "Hm.").unwrap_err();
    let outside = refused(Anchor::lines(Side::New, "config.rs", 1..=1));
    let across = refused(Anchor::lines(Side::New, "config.rs", 8..=16));
    let old_side = refused(Anchor::lines(Side::Old, "config.rs", 9..=9));

    for error in [&outside, &across, &old_side] {
        assert!(matches!(error, Error::OutsideDiff { path, .. } if path == "config.rs"), "{error:?}");
    }
    assert!(outside.to_string().contains("whole file"), "{outside}");
    svc.comment(&id, Anchor::lines(Side::New, "config.rs", 3..=9), "In the first hunk.").unwrap();
    svc.comment(&id, Anchor::lines(Side::Old, "config.rs", 14..=19), "In the second.").unwrap();
    svc.comment(&id, Anchor::file("config.rs"), "On the file.").unwrap();
    assert_eq!(svc.session(&id).unwrap().threads.len(), 4, "RT_1 and three Drafts");
}

#[test]
fn checking_an_anchor_tells_lines_outside_the_forge_s_diff_before_any_comment() {
    let (svc, _) = service_for("maria");
    let id = svc.open_pull_request(URL).unwrap();
    let session = svc.session(&id).unwrap();

    let outside = session.check_anchor(&Anchor::lines(Side::New, "config.rs", 1..=1));
    let inside = session.check_anchor(&Anchor::lines(Side::New, "config.rs", 3..=9));

    assert!(matches!(&outside, Err(Error::OutsideDiff { path, .. }) if path == "config.rs"), "{outside:?}");
    assert!(inside.is_ok(), "{inside:?}");
}

#[test]
fn lines_outside_the_forge_s_diff_fall_back_to_their_whole_file() {
    let (svc, _) = service_for("maria");
    let id = svc.open_pull_request(URL).unwrap();
    let session = svc.session(&id).unwrap();

    let outside = session.fallback_anchor(&Anchor::lines(Side::New, "config.rs", 1..=1));
    let inside = session.fallback_anchor(&Anchor::lines(Side::New, "config.rs", 3..=9));

    assert_eq!(outside, Some(Anchor::file("config.rs")));
    assert_eq!(inside, None, "the Forge takes those lines");
}
