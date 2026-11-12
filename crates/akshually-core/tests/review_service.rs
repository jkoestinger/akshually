//! Seam A: the core review service, in-process, against real throwaway git repositories.

use std::time::Duration;

use akshually_core::{
    Anchor, BinarySizes, ChangesetView, Collapsed, FileChange, Outcome, Party, ReviewRequest, Round, Service,
    SessionId, Settings, Side, Thread,
};
use akshually_testkit::{TestDirs, TestRepo};

fn change(path: &str, old: Option<&str>, new: Option<&str>) -> FileChange {
    FileChange::text(path, old, new)
}

/// A repository on branch `feature`, which has one commit of its own, while `main` moved on
/// after the branch point, plus uncommitted changes: a staged new file, an unstaged edit and
/// an unstaged deletion.
fn feature_branch_with_uncommitted_work() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("keep.txt", "untouched\n").write("edit.txt", "one\ntwo\n").write("gone.txt", "bye\n").commit_all("base");
    repo.branch("feature").write("edit.txt", "one\nTWO\n").commit_all("agent's commit");
    repo.switch("main").write("main-only.txt", "later\n").commit_all("main moves on");
    repo.switch("feature");
    repo.write("new.txt", "fresh\n").git(&["add", "new.txt"]);
    repo.write("edit.txt", "one\nTWO\nthree\n").remove("gone.txt");
    repo
}

#[test]
fn review_opens_a_session_on_the_branch_against_its_merge_base_plus_uncommitted_changes() {
    let repo = feature_branch_with_uncommitted_work();
    let svc = Service::new();

    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();

    let session = svc.session(pending.session()).expect("the Session is open");
    assert_eq!(session.repo, repo.path());
    assert_eq!(session.branch, "feature");
    assert_eq!(
        session.changeset.files,
        vec![
            change("edit.txt", Some("one\ntwo\n"), Some("one\nTWO\nthree\n")),
            change("gone.txt", Some("bye\n"), None),
            change("new.txt", None, Some("fresh\n")),
        ]
    );
}

#[test]
fn review_from_a_subdirectory_reviews_the_whole_repository() {
    let repo = TestRepo::new();
    repo.write("src/lib.rs", "a\n").commit_all("base");
    repo.write("src/lib.rs", "b\n");
    let svc = Service::new();

    let pending = svc.review(ReviewRequest::new(repo.path().join("src"))).unwrap();

    let session = svc.session(pending.session()).unwrap();
    assert_eq!(session.repo, repo.path());
    assert_eq!(session.changeset.files, vec![change("src/lib.rs", Some("a\n"), Some("b\n"))]);
}

#[test]
fn review_outside_a_repository_fails() {
    let dir = TestDirs::new();
    let svc = Service::new();

    assert!(svc.review(ReviewRequest::new(dir.path())).is_err());
    assert!(svc.sessions().is_empty());
}

#[test]
fn approving_returns_approved_to_the_waiting_review_and_ends_the_session() {
    let repo = feature_branch_with_uncommitted_work();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = pending.session().clone();

    svc.approve(&id).unwrap();

    assert_eq!(pending.wait_timeout(Duration::from_secs(1)), Some(Outcome::Approved));
    assert!(svc.session(&id).is_none());
}

#[test]
fn abandoning_returns_abandoned_to_the_waiting_review_and_ends_the_session() {
    let repo = feature_branch_with_uncommitted_work();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = pending.session().clone();

    svc.abandon(&id).unwrap();

    assert_eq!(pending.wait_timeout(Duration::from_secs(1)), Some(Outcome::Abandoned));
    assert!(svc.session(&id).is_none());
}

#[test]
fn review_blocks_until_the_human_acts() {
    let repo = feature_branch_with_uncommitted_work();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();

    assert_eq!(pending.wait_timeout(Duration::from_millis(50)), None);
}

#[test]
fn reviews_in_two_repositories_are_two_sessions() {
    let (a, b) = (feature_branch_with_uncommitted_work(), feature_branch_with_uncommitted_work());
    let svc = Service::new();

    let pa = svc.review(ReviewRequest::new(a.path())).unwrap();
    let pb = svc.review(ReviewRequest::new(b.path())).unwrap();

    assert_ne!(pa.session(), pb.session());
    assert_eq!(svc.sessions().len(), 2);
}

#[test]
fn subscribers_see_sessions_open_and_end() {
    let repo = feature_branch_with_uncommitted_work();
    let svc = Service::new();
    let (tx, rx) = std::sync::mpsc::channel();
    svc.subscribe(move |sessions| tx.send(sessions.len()).unwrap());

    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    svc.approve(pending.session()).unwrap();

    assert_eq!(rx.try_iter().collect::<Vec<_>>(), vec![1, 0]);
}

#[test]
fn acting_on_an_unknown_session_fails() {
    let repo = feature_branch_with_uncommitted_work();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    svc.approve(pending.session()).unwrap();

    assert!(svc.approve(pending.session()).is_err());
    assert!(svc.abandon(pending.session()).is_err());
}

/// A repository on branch `agent` whose `lib.rs` changed two lines and gained a third.
fn agent_branch() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("lib.rs", "fn a() {}\nfn b() {}\nfn c() {}\n").commit_all("base");
    repo.branch("agent").write("lib.rs", "fn a() {}\nfn b2() {}\nfn c2() {}\nfn d() {}\n");
    repo
}

#[test]
fn a_comment_on_a_line_range_opens_a_thread_holding_a_draft() {
    let repo = agent_branch();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();

    let thread = svc.comment(pending.session(), Anchor::lines(Side::New, "lib.rs", 2..=3), "Why rename?").unwrap();

    let session = svc.session(pending.session()).unwrap();
    assert_eq!(session.threads.len(), 1);
    let t = &session.threads[0];
    assert_eq!(t.id, thread);
    assert_eq!(t.anchor, Anchor::lines(Side::New, "lib.rs", 2..=3));
    assert_eq!(t.excerpt, "fn b2() {}\nfn c2() {}\n");
    assert_eq!(t.comments.len(), 1);
    assert_eq!(t.comments[0].author, Party::Human);
    assert_eq!(t.comments[0].body, "Why rename?");
    assert!(t.comments[0].is_draft());
    assert_eq!(pending.wait_timeout(Duration::from_millis(50)), None, "a Draft doesn't reach the agent");
}

#[test]
fn a_comment_can_anchor_to_lines_on_the_old_side() {
    let repo = agent_branch();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();

    svc.comment(pending.session(), Anchor::lines(Side::Old, "lib.rs", 3..=3), "c was fine").unwrap();

    let session = svc.session(pending.session()).unwrap();
    assert_eq!(session.threads[0].anchor, Anchor::lines(Side::Old, "lib.rs", 3..=3));
    assert_eq!(session.threads[0].excerpt, "fn c() {}\n");
}

#[test]
fn a_comment_outside_the_changeset_or_without_text_is_refused() {
    let repo = agent_branch();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = pending.session();

    assert!(svc.comment(id, Anchor::lines(Side::New, "other.rs", 1..=1), "x").is_err(), "unchanged file");
    assert!(svc.comment(id, Anchor::lines(Side::Old, "lib.rs", 4..=4), "x").is_err(), "past the old side");
    let backwards = Anchor::Lines { path: "lib.rs".into(), side: Side::New, start: 3, end: 2 };
    assert!(svc.comment(id, backwards, "x").is_err(), "backwards range");
    assert!(svc.comment(id, Anchor::lines(Side::New, "lib.rs", 0..=1), "x").is_err(), "line 0");
    assert!(svc.comment(id, Anchor::lines(Side::New, "lib.rs", 1..=1), "  \n").is_err(), "blank body");
    assert!(svc.session(id).unwrap().threads.is_empty());
}

#[test]
fn thread_ids_count_up_within_a_session() {
    let repo = agent_branch();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();

    let first = svc.comment(pending.session(), Anchor::lines(Side::New, "lib.rs", 1..=1), "one").unwrap();
    let second = svc.comment(pending.session(), Anchor::lines(Side::New, "lib.rs", 4..=4), "two").unwrap();

    assert_eq!((first.as_str(), second.as_str()), ("T1", "T2"));
}

fn round(outcome: Option<Outcome>) -> Round {
    match outcome {
        Some(Outcome::Round(round)) => round,
        other => panic!("expected a Round, got {other:?}"),
    }
}

#[test]
fn handing_over_sends_every_draft_to_the_waiting_review_as_round_1() {
    let repo = agent_branch();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = pending.session().clone();
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 2..=3), "Why rename?").unwrap();
    svc.comment(&id, Anchor::lines(Side::Old, "lib.rs", 3..=3), "c was fine").unwrap();

    assert_eq!(svc.hand_over_round(&id).unwrap(), 1);

    let round = round(pending.wait_timeout(Duration::from_secs(1)));
    assert_eq!(round.number, 1);
    let sent: Vec<_> = round.threads.iter().map(|t| (t.id.as_str(), t.comments[0].body.as_str())).collect();
    assert_eq!(sent, vec![("T1", "Why rename?"), ("T2", "c was fine")]);
    assert!(round.threads.iter().all(|t| t.comments.iter().all(|c| c.round == Some(1))));
    let session = svc.session(&id).expect("a Round doesn't end the Session");
    assert_eq!(session.round, 2);
    assert!(session.threads.iter().flat_map(|t| &t.comments).all(|c| !c.is_draft()));
}

#[test]
fn handing_over_without_drafts_is_refused() {
    let repo = agent_branch();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();

    assert!(svc.hand_over_round(pending.session()).is_err());
    assert_eq!(pending.wait_timeout(Duration::from_millis(50)), None);
    assert_eq!(svc.session(pending.session()).unwrap().round, 1);
}

#[test]
fn the_next_review_reuses_the_session_with_a_fresh_changeset_and_the_same_threads() {
    let repo = agent_branch();
    let svc = Service::new();
    let first = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = first.session().clone();
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 2..=3), "Why rename?").unwrap();
    svc.hand_over_round(&id).unwrap();
    round(first.wait_timeout(Duration::from_secs(1)));
    let threads_before = svc.session(&id).unwrap().threads;

    repo.write("lib.rs", "fn a2() {}\nfn b2() {}\nfn c2() {}\nfn d() {}\n");
    let second = svc.review(ReviewRequest::new(repo.path())).unwrap();

    assert_eq!(second.session(), &id);
    assert_eq!(svc.sessions().len(), 1);
    let session = svc.session(&id).unwrap();
    assert_eq!(session.changeset.files[0].new.as_deref(), Some("fn a2() {}\nfn b2() {}\nfn c2() {}\nfn d() {}\n"));
    assert_eq!(session.threads, threads_before);
    assert_eq!(second.wait_timeout(Duration::from_millis(50)), None, "it waits for the next Round");
}

#[test]
fn rounds_count_up_and_each_carries_only_its_own_drafts() {
    let repo = agent_branch();
    let svc = Service::new();
    let first = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = first.session().clone();
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 2..=3), "Why rename?").unwrap();
    svc.hand_over_round(&id).unwrap();
    round(first.wait_timeout(Duration::from_secs(1)));

    let second = svc.review(ReviewRequest::new(repo.path())).unwrap();
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 4..=4), "Add a test for d").unwrap();
    assert_eq!(svc.hand_over_round(&id).unwrap(), 2);

    let round2 = round(second.wait_timeout(Duration::from_secs(1)));
    assert_eq!(round2.number, 2);
    assert_eq!(round2.threads.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), vec!["T2"]);
    assert_eq!(svc.session(&id).unwrap().round, 3);
}

#[test]
fn a_round_handed_over_while_no_review_waits_goes_to_the_next_call() {
    let repo = agent_branch();
    let svc = Service::new();
    let first = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = first.session().clone();
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 1..=1), "one").unwrap();
    svc.hand_over_round(&id).unwrap();
    round(first.wait_timeout(Duration::from_secs(1)));
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 4..=4), "two").unwrap();

    svc.hand_over_round(&id).unwrap();
    let second = svc.review(ReviewRequest::new(repo.path())).unwrap();

    assert_eq!(round(second.wait_timeout(Duration::from_secs(1))).number, 2);
}

/// One re-anchoring case: the agent's `lib.rs` when the human comments on `anchor`, the file
/// after the agent addresses the Round, and where the Thread should be then (`None`: Outdated).
struct Reanchoring {
    name: &'static str,
    before: &'static str,
    anchor: Anchor,
    after: &'static str,
    expected: Option<Anchor>,
}

/// Comments on `case.anchor`, hands the Round over, lets the agent rewrite `lib.rs`, and
/// returns the Thread as the next `review` call leaves it.
fn reanchored(case: &Reanchoring) -> Thread {
    let repo = TestRepo::new();
    repo.write("lib.rs", "fn base() {}\n").commit_all("base");
    repo.branch("agent").write("lib.rs", case.before);
    let svc = Service::new();
    let first = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = first.session().clone();
    svc.comment(&id, case.anchor.clone(), "Look here").unwrap();
    svc.hand_over_round(&id).unwrap();
    round(first.wait_timeout(Duration::from_secs(1)));

    repo.write("lib.rs", case.after);
    svc.review(ReviewRequest::new(repo.path())).unwrap();

    svc.session(&id).unwrap().threads.remove(0)
}

#[test]
fn threads_are_re_anchored_by_content_on_the_next_changeset() {
    let cases = [
        Reanchoring {
            name: "lines moved down",
            before: "fn a() {}\nfn b() {}\nfn c() {}\n",
            anchor: Anchor::lines(Side::New, "lib.rs", 2..=3),
            after: "use x;\n\nfn a() {}\nfn b() {}\nfn c() {}\n",
            expected: Some(Anchor::lines(Side::New, "lib.rs", 4..=5)),
        },
        Reanchoring {
            name: "lines moved up",
            before: "use x;\n\nfn a() {}\nfn b() {}\n",
            anchor: Anchor::lines(Side::New, "lib.rs", 4..=4),
            after: "fn a() {}\nfn b() {}\n",
            expected: Some(Anchor::lines(Side::New, "lib.rs", 2..=2)),
        },
        Reanchoring {
            name: "lines untouched while others changed",
            before: "fn a() {}\nfn b() {}\n",
            anchor: Anchor::lines(Side::New, "lib.rs", 1..=1),
            after: "fn a() {}\nfn b2() {}\n",
            expected: Some(Anchor::lines(Side::New, "lib.rs", 1..=1)),
        },
        Reanchoring {
            name: "moved, re-indented and lightly edited",
            before: "fn f() {\n    let a = 1;\n    let b = 2;\n    a + b\n}\n",
            anchor: Anchor::lines(Side::New, "lib.rs", 2..=4),
            after: "mod m {\n    fn f() {\n        let a = 1;\n        let b = 3;\n        a + b\n    }\n}\n",
            expected: Some(Anchor::lines(Side::New, "lib.rs", 3..=5)),
        },
        Reanchoring {
            name: "repeated code: the copy nearest the old place",
            before: "x();\nfn a() {}\ny();\nfn a() {}\n",
            anchor: Anchor::lines(Side::New, "lib.rs", 4..=4),
            after: "x();\nfn a() {}\ny();\nz();\nfn a() {}\n",
            expected: Some(Anchor::lines(Side::New, "lib.rs", 5..=5)),
        },
        Reanchoring {
            name: "old side, unchanged base",
            before: "fn a() {}\n",
            anchor: Anchor::lines(Side::Old, "lib.rs", 1..=1),
            after: "fn a() {}\nfn b() {}\n",
            expected: Some(Anchor::lines(Side::Old, "lib.rs", 1..=1)),
        },
        Reanchoring {
            name: "rewritten beyond recognition",
            before: "fn f() {\n    let a = 1;\n    let b = 2;\n    a + b\n}\n",
            anchor: Anchor::lines(Side::New, "lib.rs", 2..=4),
            after: "fn f() {\n    3\n}\n",
            expected: None,
        },
        Reanchoring {
            name: "only brackets and blank lines left alike",
            before: "fn f() {\n    one();\n}\n\nfn g() {}\n",
            anchor: Anchor::lines(Side::New, "lib.rs", 2..=4),
            after: "fn f() {\n    two();\n}\n\nfn h() {}\n",
            expected: None,
        },
        Reanchoring {
            name: "the file no longer changed",
            before: "fn a() {}\n",
            anchor: Anchor::lines(Side::New, "lib.rs", 1..=1),
            after: "fn base() {}\n",
            expected: None,
        },
    ];
    for case in &cases {
        let thread = reanchored(case);
        match &case.expected {
            Some(anchor) => {
                assert_eq!(&thread.anchor, anchor, "{}", case.name);
                assert!(!thread.outdated, "{}: not Outdated", case.name);
            }
            None => assert!(thread.outdated, "{}: Outdated", case.name),
        }
    }
}

#[test]
fn an_outdated_thread_stays_in_the_session_with_its_code_and_comments() {
    let repo = agent_branch();
    let svc = Service::new();
    let first = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = first.session().clone();
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 2..=3), "Why rename?").unwrap();
    svc.hand_over_round(&id).unwrap();
    round(first.wait_timeout(Duration::from_secs(1)));

    repo.write("lib.rs", "fn a() {}\nfn everything_else() {}\n");
    svc.review(ReviewRequest::new(repo.path())).unwrap();

    let threads = svc.session(&id).unwrap().threads;
    assert_eq!(threads.len(), 1, "still visible");
    let t = &threads[0];
    assert!(t.outdated);
    assert_eq!(t.excerpt, "fn b2() {}\nfn c2() {}\n", "the code it was written against");
    assert_eq!(t.anchor, Anchor::lines(Side::New, "lib.rs", 2..=3), "where it last was");
    assert_eq!(t.comments.iter().map(|c| c.body.as_str()).collect::<Vec<_>>(), vec!["Why rename?"]);
}

#[test]
fn an_outdated_thread_is_placed_again_when_its_code_comes_back() {
    let repo = agent_branch();
    let svc = Service::new();
    let first = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = first.session().clone();
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 2..=3), "Why rename?").unwrap();
    svc.hand_over_round(&id).unwrap();
    round(first.wait_timeout(Duration::from_secs(1)));
    repo.write("lib.rs", "fn a() {}\nfn everything_else() {}\n");
    svc.review(ReviewRequest::new(repo.path())).unwrap();
    assert!(svc.session(&id).unwrap().threads[0].outdated);

    repo.write("lib.rs", "// back\nfn a() {}\nfn b2() {}\nfn c2() {}\n");
    svc.review(ReviewRequest::new(repo.path())).unwrap();

    let t = &svc.session(&id).unwrap().threads[0];
    assert!(!t.outdated);
    assert_eq!(t.anchor, Anchor::lines(Side::New, "lib.rs", 3..=4));
}

#[test]
fn review_with_a_base_shows_the_changes_since_its_merge_base() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n").commit_all("base");
    repo.branch("topic").write("a.txt", "topic\n").commit_all("topic work");
    repo.branch("feature").write("b.txt", "feature\n").commit_all("feature work");
    let svc = Service::new();

    let pending = svc.review(ReviewRequest::new(repo.path()).base("topic")).unwrap();

    let session = svc.session(pending.session()).unwrap();
    assert_eq!(session.changeset.files, vec![change("b.txt", None, Some("feature\n"))]);
}

#[test]
fn review_with_an_unknown_base_fails() {
    let repo = feature_branch_with_uncommitted_work();
    let svc = Service::new();

    assert!(svc.review(ReviewRequest::new(repo.path()).base("no-such-ref")).is_err());
}

/// `main` with three commits (one, two, three), each adding the file named after it, plus an
/// uncommitted edit.
fn three_commits_and_uncommitted_work() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("one.txt", "1\n").commit_all("one");
    repo.write("two.txt", "2\n").commit_all("two");
    repo.write("three.txt", "3\n").commit_all("three");
    repo.write("one.txt", "uncommitted\n");
    repo
}

#[test]
fn review_of_a_commit_range_shows_exactly_those_commits() {
    let repo = three_commits_and_uncommitted_work();
    let svc = Service::new();

    let pending = svc.review(ReviewRequest::new(repo.path()).range("HEAD~2..HEAD~1")).unwrap();

    let session = svc.session(pending.session()).unwrap();
    assert_eq!(session.changeset.base, repo.git(&["rev-parse", "HEAD~2"]));
    assert_eq!(session.changeset.files, vec![change("two.txt", None, Some("2\n"))]);
}

#[test]
fn review_of_a_three_dot_range_shows_the_changes_since_the_merge_base() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n").commit_all("base");
    repo.branch("feature").write("b.txt", "feature\n").commit_all("feature work");
    repo.switch("main").write("a.txt", "main moved\n").commit_all("main moves on");
    let svc = Service::new();

    let pending = svc.review(ReviewRequest::new(repo.path()).range("main...feature")).unwrap();

    let session = svc.session(pending.session()).unwrap();
    assert_eq!(session.changeset.files, vec![change("b.txt", None, Some("feature\n"))]);
}

#[test]
fn review_with_both_a_base_and_a_range_fails() {
    let repo = three_commits_and_uncommitted_work();
    let svc = Service::new();

    assert!(svc.review(ReviewRequest::new(repo.path()).base("main").range("HEAD~1..HEAD")).is_err());
    assert!(svc.review(ReviewRequest::new(repo.path()).range("HEAD~1")).is_err(), "a range needs `..`");
}

/// A committed `.gitignore` and `tracked.txt`, then an edit to `tracked.txt`, a file the agent
/// forgot to `git add`, and an ignored build output.
fn repo_with_untracked_and_ignored_files() -> TestRepo {
    let repo = TestRepo::new();
    repo.write(".gitignore", "target/\n").write("tracked.txt", "a\n").commit_all("base");
    repo.write("tracked.txt", "b\n").write("forgot.txt", "new\n").write("target/out.bin", "build\n");
    repo
}

fn paths(svc: &Service, pending: &akshually_core::PendingReview) -> Vec<String> {
    svc.session(pending.session()).unwrap().changeset.files.into_iter().map(|f| f.path).collect()
}

#[test]
fn untracked_non_ignored_files_are_included_by_default() {
    let repo = repo_with_untracked_and_ignored_files();
    let status_before = repo.git(&["status", "--porcelain"]);
    let svc = Service::new();

    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();

    assert_eq!(paths(&svc, &pending), ["forgot.txt", "tracked.txt"]);
    assert_eq!(repo.git(&["status", "--porcelain"]), status_before, "the repository's index is untouched");
}

#[test]
fn the_untracked_setting_excludes_untracked_files() {
    let repo = repo_with_untracked_and_ignored_files();
    let svc = Service::new();
    svc.set_settings(Settings { untracked: false });

    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();

    assert_eq!(paths(&svc, &pending), ["tracked.txt"]);
}

#[test]
fn a_review_call_overrides_the_untracked_setting() {
    let (repo, other) = (repo_with_untracked_and_ignored_files(), repo_with_untracked_and_ignored_files());
    let excluding = Service::new();
    excluding.set_settings(Settings { untracked: false });
    let including = Service::new();

    let included = excluding.review(ReviewRequest::new(repo.path()).untracked(true)).unwrap();
    let excluded = including.review(ReviewRequest::new(other.path()).untracked(false)).unwrap();

    assert_eq!(paths(&excluding, &included), ["forgot.txt", "tracked.txt"]);
    assert_eq!(paths(&including, &excluded), ["tracked.txt"]);
}

/// A repository on `feature` where the human has uncommitted work in progress (an edit to
/// `shared.txt` and an untracked `notes.txt`) when the agent runs `start`.
fn human_work_in_progress() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("shared.txt", "1\n2\n").commit_all("base");
    repo.branch("feature");
    repo.write("shared.txt", "1 (human)\n2\n").write("notes.txt", "human notes\n");
    repo
}

#[test]
fn after_start_the_session_shows_only_the_agents_changes_since_the_session_base() {
    let repo = human_work_in_progress();
    let svc = Service::new();

    svc.start(repo.path()).unwrap();
    repo.write("shared.txt", "1 (human)\n2 (agent)\n").write("agent.txt", "agent\n");
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();

    let session = svc.session(pending.session()).unwrap();
    assert_eq!(session.view, ChangesetView::SinceSessionBase);
    assert_eq!(
        session.shown().files,
        vec![
            change("agent.txt", None, Some("agent\n")),
            change("shared.txt", Some("1 (human)\n2\n"), Some("1 (human)\n2 (agent)\n")),
        ]
    );
}

#[test]
fn the_human_can_toggle_to_the_full_changeset_and_back() {
    let repo = human_work_in_progress();
    let svc = Service::new();
    svc.start(repo.path()).unwrap();
    repo.write("agent.txt", "agent\n");
    let id = svc.review(ReviewRequest::new(repo.path())).unwrap().session().clone();

    svc.show(&id, ChangesetView::Full).unwrap();

    let full = svc.session(&id).unwrap();
    assert_eq!(full.view, ChangesetView::Full);
    assert_eq!(
        full.shown().files,
        vec![
            change("agent.txt", None, Some("agent\n")),
            change("notes.txt", None, Some("human notes\n")),
            change("shared.txt", Some("1\n2\n"), Some("1 (human)\n2\n")),
        ]
    );
    svc.show(&id, ChangesetView::SinceSessionBase).unwrap();
    assert_eq!(svc.session(&id).unwrap().shown().files.len(), 1);
}

#[test]
fn without_start_the_session_shows_the_full_changeset() {
    let repo = human_work_in_progress();
    let svc = Service::new();

    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();

    let session = svc.session(pending.session()).unwrap();
    assert_eq!(session.view, ChangesetView::Full);
    assert_eq!(session.since_session_base, None);
    assert_eq!(session.shown().files.len(), 2);
}

#[test]
fn a_comment_anchors_to_the_changeset_the_human_is_looking_at() {
    let repo = human_work_in_progress();
    let svc = Service::new();
    svc.start(repo.path()).unwrap();
    repo.write("shared.txt", "1 (human)\n2 (agent)\n");
    let id = svc.review(ReviewRequest::new(repo.path())).unwrap().session().clone();

    let old_line_1 = || Anchor::lines(Side::Old, "shared.txt", 1..=1);
    svc.comment(&id, old_line_1(), "since the Session base").unwrap();
    svc.show(&id, ChangesetView::Full).unwrap();
    svc.comment(&id, old_line_1(), "in the full Changeset").unwrap();

    let excerpts: Vec<String> = svc.session(&id).unwrap().threads.into_iter().map(|t| t.excerpt).collect();
    assert_eq!(excerpts, ["1 (human)\n", "1\n"]);
}

#[test]
fn excluded_untracked_files_stay_out_of_the_view_since_the_session_base() {
    let repo = human_work_in_progress();
    let svc = Service::new();
    svc.start(repo.path()).unwrap();
    repo.write("agent.txt", "agent\n").write("notes.txt", "human notes, edited\n");

    let pending = svc.review(ReviewRequest::new(repo.path()).untracked(false)).unwrap();

    let session = svc.session(pending.session()).unwrap();
    assert_eq!(session.shown().files, vec![]);
    assert_eq!(paths(&svc, &pending), ["shared.txt"]);
}

#[test]
fn toggling_the_view_re_anchors_threads_in_the_shown_changeset() {
    let repo = TestRepo::new();
    repo.write("lib.rs", "fn x() {}\n").commit_all("base");
    repo.branch("feature").write("lib.rs", "// human\nfn x() {}\n");
    let svc = Service::new();
    svc.start(repo.path()).unwrap();
    repo.write("lib.rs", "// human\nfn y() {}\n");
    let id = svc.review(ReviewRequest::new(repo.path())).unwrap().session().clone();
    // Line 2 on the old side since the Session base is line 1 at the merge-base.
    svc.comment(&id, Anchor::lines(Side::Old, "lib.rs", 2..=2), "Keep x").unwrap();

    svc.show(&id, ChangesetView::Full).unwrap();

    let t = &svc.session(&id).unwrap().threads[0];
    assert_eq!(t.anchor, Anchor::lines(Side::Old, "lib.rs", 1..=1));
    assert!(!t.outdated);
    svc.show(&id, ChangesetView::SinceSessionBase).unwrap();
    assert_eq!(svc.session(&id).unwrap().threads[0].anchor, Anchor::lines(Side::Old, "lib.rs", 2..=2));
}

#[test]
fn start_outside_a_repository_fails() {
    let dir = TestDirs::new();

    assert!(Service::new().start(dir.path()).is_err());
}

#[test]
fn binary_files_are_summarised_by_their_sizes() {
    let repo = TestRepo::new();
    repo.write_bytes("image.png", &[0x89, b'P', b'N', b'G', 0, 1, 2]).commit_all("base");
    repo.write_bytes("image.png", &[0x89, b'P', b'N', b'G', 0, 1, 2, 3, 4, 5]).write_bytes("new.bin", &[0; 4]);
    let svc = Service::new();

    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();

    let files = svc.session(pending.session()).unwrap().changeset.files;
    let binary = |path: &str, old: Option<&str>, new: Option<&str>, sizes: (Option<u64>, Option<u64>)| FileChange {
        binary: Some(BinarySizes { old: sizes.0, new: sizes.1 }),
        ..change(path, old, new)
    };
    assert_eq!(
        files,
        vec![
            binary("image.png", Some(""), Some(""), (Some(7), Some(10))),
            binary("new.bin", None, Some(""), (None, Some(4)))
        ]
    );
}

fn collapsed(svc: &Service, pending: &akshually_core::PendingReview) -> Vec<(String, Option<Collapsed>)> {
    let files = svc.session(pending.session()).unwrap().changeset.files;
    files.into_iter().map(|f| (f.path, f.collapsed)).collect()
}

#[test]
fn generated_files_are_collapsed_with_their_text_kept_for_expanding() {
    let repo = TestRepo::new();
    repo.write(".gitattributes", "api/*.json linguist-generated\nyarn.lock linguist-generated=false\n");
    repo.write("Cargo.lock", "# old\n").write("yarn.lock", "old\n").write("api/schema.json", "{}\n");
    repo.write("gen.rs", "// @generated by a tool\nold\n").write("main.rs", "old\n").commit_all("base");
    for file in ["Cargo.lock", "yarn.lock", "api/schema.json", "main.rs"] {
        repo.write(file, "new\n");
    }
    repo.write("gen.rs", "// @generated by a tool\nnew\n");
    let svc = Service::new();

    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();

    let generated = Some(Collapsed::Generated);
    assert_eq!(
        collapsed(&svc, &pending),
        [
            ("Cargo.lock".to_owned(), generated),
            ("api/schema.json".to_owned(), generated),
            ("gen.rs".to_owned(), generated),
            ("main.rs".to_owned(), None),
            ("yarn.lock".to_owned(), None),
        ]
    );
    let lockfile = svc.session(pending.session()).unwrap().changeset.files.remove(0);
    assert_eq!((lockfile.old.as_deref(), lockfile.new.as_deref()), (Some("# old\n"), Some("new\n")));
}

#[test]
fn huge_files_are_collapsed() {
    let repo = TestRepo::new();
    repo.write("small.txt", "a\n").write("huge.txt", "a\n").commit_all("base");
    repo.write("small.txt", &"b\n".repeat(100)).write("huge.txt", &"b\n".repeat(5000));
    let svc = Service::new();

    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();

    assert_eq!(
        collapsed(&svc, &pending),
        [("huge.txt".to_owned(), Some(Collapsed::Large)), ("small.txt".to_owned(), None)]
    );
}

/// A Session in its second Round: the agent changed `lib.rs` and `other.rs` for Round 1, then,
/// in response, changed `lib.rs` again and added `extra.rs` before calling `review` again.
fn second_round(svc: &Service) -> (TestRepo, akshually_core::PendingReview) {
    let repo = TestRepo::new();
    repo.write("lib.rs", "fn a() {}\n").write("other.rs", "fn o() {}\n").commit_all("base");
    repo.branch("agent").write("lib.rs", "fn a() {}\nfn d() {}\n").write("other.rs", "fn o2() {}\n");
    let first = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = first.session().clone();
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 2..=2), "Add a test for d").unwrap();
    svc.hand_over_round(&id).unwrap();
    round(first.wait_timeout(Duration::from_secs(1)));
    repo.write("lib.rs", "fn a() {}\nfn d() {}\n#[test]\nfn d_works() {}\n").write("extra.rs", "fn e() {}\n");
    repo.git(&["add", "extra.rs"]);
    let second = svc.review(ReviewRequest::new(repo.path())).unwrap();
    (repo, second)
}

#[test]
fn files_can_be_ticked_as_viewed_and_unticked() {
    let repo = agent_branch();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = pending.session();

    svc.mark_viewed(id, "lib.rs", true).unwrap();
    assert!(svc.session(id).unwrap().viewed.contains("lib.rs"));

    svc.mark_viewed(id, "lib.rs", false).unwrap();
    assert!(svc.session(id).unwrap().viewed.is_empty());

    assert!(svc.mark_viewed(id, "unchanged.rs", true).is_err(), "not in the Changeset");
    assert!(svc.session(id).unwrap().viewed.is_empty());
}

#[test]
fn viewed_ticks_persist_across_rounds_on_unchanged_files_and_clear_on_changed_ones() {
    let repo = TestRepo::new();
    repo.write("lib.rs", "fn a() {}\n").write("other.rs", "fn o() {}\n").commit_all("base");
    repo.branch("agent").write("lib.rs", "fn a() {}\nfn d() {}\n").write("other.rs", "fn o2() {}\n");
    let svc = Service::new();
    let first = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = first.session().clone();
    svc.mark_viewed(&id, "lib.rs", true).unwrap();
    svc.mark_viewed(&id, "other.rs", true).unwrap();
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 2..=2), "Add a test for d").unwrap();
    svc.hand_over_round(&id).unwrap();
    round(first.wait_timeout(Duration::from_secs(1)));

    repo.write("lib.rs", "fn a() {}\nfn d() {}\n#[test]\nfn d_works() {}\n");
    let _second = svc.review(ReviewRequest::new(repo.path())).unwrap();

    let viewed = svc.session(&id).unwrap().viewed;
    assert_eq!(viewed.iter().map(String::as_str).collect::<Vec<_>>(), ["other.rs"]);
}

#[test]
fn from_round_2_the_default_view_shows_only_the_changes_since_the_last_round() {
    let svc = Service::new();

    let (_repo, second) = second_round(&svc);

    let session = svc.session(second.session()).unwrap();
    assert_eq!(session.view, ChangesetView::SinceLastRound);
    assert_eq!(
        session.shown().files,
        vec![
            change("extra.rs", None, Some("fn e() {}\n")),
            change("lib.rs", Some("fn a() {}\nfn d() {}\n"), Some("fn a() {}\nfn d() {}\n#[test]\nfn d_works() {}\n")),
        ]
    );
}

#[test]
fn round_1_shows_the_full_changeset() {
    let repo = agent_branch();
    let svc = Service::new();

    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();

    let session = svc.session(pending.session()).unwrap();
    assert_eq!(session.view, ChangesetView::Full);
    assert_eq!(session.shown(), &session.changeset);

    svc.show(pending.session(), ChangesetView::SinceLastRound).unwrap();
    let session = svc.session(pending.session()).unwrap();
    assert_eq!(session.shown(), &session.changeset, "there's no last Round yet");
}

fn shown_paths(changeset: &akshually_core::Changeset) -> Vec<&str> {
    changeset.files.iter().map(|f| f.path.as_str()).collect()
}

#[test]
fn the_toggle_switches_to_the_full_changeset_and_back() {
    let svc = Service::new();
    let (_repo, second) = second_round(&svc);
    let id = second.session();

    svc.show(id, ChangesetView::Full).unwrap();
    let full = svc.session(id).unwrap();
    assert_eq!(shown_paths(full.shown()), ["extra.rs", "lib.rs", "other.rs"]);
    assert_eq!(full.shown().files[1].old.as_deref(), Some("fn a() {}\n"), "against the base again");

    svc.show(id, ChangesetView::SinceLastRound).unwrap();
    assert_eq!(shown_paths(svc.session(id).unwrap().shown()), ["extra.rs", "lib.rs"]);
}

#[test]
fn a_file_the_agent_reverted_since_the_last_round_shows_as_changed_back() {
    let svc = Service::new();
    let (repo, second) = second_round(&svc);
    drop(second);

    repo.write("other.rs", "fn o() {}\n");
    let third = svc.review(ReviewRequest::new(repo.path())).unwrap();

    let session = svc.session(third.session()).unwrap();
    assert!(session.shown().files.contains(&change("other.rs", Some("fn o2() {}\n"), Some("fn o() {}\n"))));
}

// Persistence: Sessions survive restarts, shown by building a new service on the same state dir.

/// Opens a Session on `repo`, hands Round 1 over to the agent, and leaves a Draft for Round 2.
fn session_mid_review(svc: &Service, repo: &TestRepo) -> SessionId {
    let first = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = first.session().clone();
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 2..=3), "Why rename?").unwrap();
    svc.hand_over_round(&id).unwrap();
    round(first.wait_timeout(Duration::from_secs(1)));
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 4..=4), "Add a test for d").unwrap();
    id
}

#[test]
fn a_new_service_on_the_same_state_dir_restores_every_open_session() {
    let dirs = TestDirs::new();
    let (repo, other) = (agent_branch(), agent_branch());
    let svc = Service::open(dirs.state()).unwrap();
    let id = session_mid_review(&svc, &repo);
    svc.mark_viewed(&id, "lib.rs", true).unwrap();
    let _waiting = svc.review(ReviewRequest::new(other.path())).unwrap();
    let before = svc.sessions();
    assert!(before[0].last_round.is_some() && !before[0].viewed.is_empty() && before[1].attention);
    drop(svc);

    let restored = Service::open(dirs.state()).unwrap();

    assert_eq!(restored.sessions(), before);
}

#[test]
fn after_a_restart_the_agents_next_review_attaches_to_the_restored_session() {
    let dirs = TestDirs::new();
    let repo = agent_branch();
    let svc = Service::open(dirs.state()).unwrap();
    let id = session_mid_review(&svc, &repo);
    let threads = svc.session(&id).unwrap().threads;
    drop(svc);

    let restored = Service::open(dirs.state()).unwrap();
    let pending = restored.review(ReviewRequest::new(repo.path())).unwrap();

    assert_eq!(pending.session(), &id);
    assert_eq!(restored.sessions().len(), 1);
    assert_eq!(restored.session(&id).unwrap().threads, threads);
    restored.hand_over_round(&id).unwrap();
    let round2 = round(pending.wait_timeout(Duration::from_secs(1)));
    assert_eq!((round2.number, round2.threads[0].id.as_str()), (2, "T2"));
}

#[test]
fn a_round_queued_before_a_restart_goes_to_the_next_review() {
    let dirs = TestDirs::new();
    let repo = agent_branch();
    let svc = Service::open(dirs.state()).unwrap();
    let id = session_mid_review(&svc, &repo);
    svc.hand_over_round(&id).unwrap();
    drop(svc);

    let restored = Service::open(dirs.state()).unwrap();
    let pending = restored.review(ReviewRequest::new(repo.path())).unwrap();

    assert_eq!(round(pending.wait_timeout(Duration::from_secs(1))).number, 2);
}

#[test]
fn sessions_opened_after_a_restart_get_new_ids_and_thread_ids_keep_counting() {
    let dirs = TestDirs::new();
    let (repo, other) = (agent_branch(), agent_branch());
    let svc = Service::open(dirs.state()).unwrap();
    let id = session_mid_review(&svc, &repo);
    drop(svc);

    let restored = Service::open(dirs.state()).unwrap();
    let new = restored.review(ReviewRequest::new(other.path())).unwrap();
    let thread = restored.comment(&id, Anchor::lines(Side::New, "lib.rs", 1..=1), "three").unwrap();

    assert_ne!(new.session(), &id);
    assert_eq!(restored.sessions().len(), 2);
    assert_eq!(thread.as_str(), "T3");
}

#[test]
fn approved_and_abandoned_sessions_are_not_restored() {
    let dirs = TestDirs::new();
    let (approved, abandoned, open) = (agent_branch(), agent_branch(), agent_branch());
    let svc = Service::open(dirs.state()).unwrap();
    let approved = session_mid_review(&svc, &approved);
    let abandoned = session_mid_review(&svc, &abandoned);
    let open = session_mid_review(&svc, &open);
    svc.approve(&approved).unwrap();
    svc.abandon(&abandoned).unwrap();
    drop(svc);

    let restored = Service::open(dirs.state()).unwrap();

    assert_eq!(restored.sessions().iter().map(|s| &s.id).collect::<Vec<_>>(), vec![&open]);
}

#[test]
fn a_persisted_session_writes_nothing_inside_the_repository() {
    let dirs = TestDirs::new();
    let repo = agent_branch();
    let everything = ["status", "--porcelain", "--ignored", "--untracked-files=all"];
    let before = repo.git(&everything);
    let svc = Service::open(dirs.state()).unwrap();

    session_mid_review(&svc, &repo);
    drop(svc);
    Service::open(dirs.state()).unwrap().review(ReviewRequest::new(repo.path())).unwrap();

    assert_eq!(repo.git(&everything), before);
}

#[test]
fn a_session_base_recorded_before_a_restart_still_applies_to_the_first_review() {
    let dirs = TestDirs::new();
    let repo = human_work_in_progress();
    Service::open(dirs.state()).unwrap().start(repo.path()).unwrap();
    repo.write("agent.txt", "agent\n");

    let restored = Service::open(dirs.state()).unwrap();
    let pending = restored.review(ReviewRequest::new(repo.path())).unwrap();

    let session = restored.session(pending.session()).unwrap();
    assert_eq!(session.view, ChangesetView::SinceSessionBase);
    assert_eq!(session.shown().files, vec![change("agent.txt", None, Some("agent\n"))]);
}

#[test]
fn a_restored_session_keeps_its_session_base_and_the_changeset_the_human_chose() {
    let dirs = TestDirs::new();
    let repo = human_work_in_progress();
    let svc = Service::open(dirs.state()).unwrap();
    svc.start(repo.path()).unwrap();
    repo.write("agent.txt", "agent\n");
    let id = svc.review(ReviewRequest::new(repo.path())).unwrap().session().clone();
    svc.show(&id, ChangesetView::Full).unwrap();
    let before = svc.session(&id).unwrap();
    drop(svc);

    let restored = Service::open(dirs.state()).unwrap();
    assert_eq!(restored.session(&id).unwrap(), before);
    repo.write("agent.txt", "agent, again\n");
    restored.review(ReviewRequest::new(repo.path())).unwrap();

    let session = restored.session(&id).unwrap();
    assert_eq!(session.view, ChangesetView::Full);
    let since = session.since_session_base.expect("the Session base survived").files;
    assert_eq!(since, vec![change("agent.txt", None, Some("agent, again\n"))]);
}
