//! Seam A: the core review service, in-process, against real throwaway git repositories.

use std::time::Duration;

use akshually_core::{Anchor, FileChange, Outcome, Party, ReviewRequest, Round, Service, Side, Thread};
use akshually_testkit::{TestDirs, TestRepo};

fn change(path: &str, old: Option<&str>, new: Option<&str>) -> FileChange {
    FileChange { path: path.into(), old: old.map(Into::into), new: new.map(Into::into) }
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
