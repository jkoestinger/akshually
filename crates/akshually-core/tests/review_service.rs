//! Seam A: the core review service, in-process, against real throwaway git repositories.

use std::time::Duration;

use akshually_core::{
    Anchor, BinarySizes, ChangesetView, Collapsed, Error, FileChange, Outcome, Party, PlacedThread, ReviewRequest,
    Round, Service, SessionId, Settings, Side, Source, Thread, ThreadId, ThreadRef, Turn,
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
    assert_eq!(session.repo(), Some(repo.path()));
    assert_eq!(session.branch(), "feature");
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
    assert_eq!(session.repo(), Some(repo.path()));
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
    drop(first);
    svc.take_turn_back(&id).unwrap();
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 4..=4), "two").unwrap();

    svc.hand_over_round(&id).unwrap();
    let second = svc.review(ReviewRequest::new(repo.path())).unwrap();

    assert_eq!(round(second.wait_timeout(Duration::from_secs(1))).number, 2);
    assert_eq!(svc.session(&id).unwrap().turn, Turn::Agent, "the agent got its Round at once");
}

#[test]
fn the_turn_passes_to_the_agent_with_a_round_and_back_to_the_human_with_the_next_review() {
    let repo = agent_branch();
    let svc = Service::new();
    let first = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = first.session().clone();
    assert_eq!(svc.session(&id).unwrap().turn, Turn::Human, "the agent asked for review");

    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 1..=1), "one").unwrap();
    svc.hand_over_round(&id).unwrap();
    assert_eq!(svc.session(&id).unwrap().turn, Turn::Agent);
    round(first.wait_timeout(Duration::from_secs(1)));
    assert_eq!(svc.session(&id).unwrap().turn, Turn::Agent, "the agent is addressing the Round");

    let _second = svc.review(ReviewRequest::new(repo.path())).unwrap();
    assert_eq!(svc.session(&id).unwrap().turn, Turn::Human);
}

#[test]
fn drafts_written_during_the_agents_turn_go_out_with_the_next_round() {
    let repo = agent_branch();
    let svc = Service::new();
    let first = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = first.session().clone();
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 1..=1), "one").unwrap();
    svc.hand_over_round(&id).unwrap();
    round(first.wait_timeout(Duration::from_secs(1)));

    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 4..=4), "thought of this meanwhile").unwrap();
    assert!(matches!(svc.hand_over_round(&id), Err(Error::NotYourTurn)), "it's the agent's Turn");

    let second = svc.review(ReviewRequest::new(repo.path())).unwrap();
    assert_eq!(second.wait_timeout(Duration::from_millis(50)), None, "the Draft waits for the human");
    svc.hand_over_round(&id).unwrap();
    let round2 = round(second.wait_timeout(Duration::from_secs(1)));
    assert_eq!(round2.number, 2);
    assert_eq!(round2.threads[0].comments[0].body, "thought of this meanwhile");
}

#[test]
fn taking_the_turn_back_lets_the_human_go_on_while_the_agent_is_quiet() {
    let repo = agent_branch();
    let svc = Service::new();
    let first = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = first.session().clone();
    assert!(matches!(svc.take_turn_back(&id), Err(Error::AlreadyYourTurn)), "a new Session is the human's Turn");
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 1..=1), "one").unwrap();
    svc.hand_over_round(&id).unwrap();
    round(first.wait_timeout(Duration::from_secs(1)));

    svc.take_turn_back(&id).unwrap();

    assert_eq!(svc.session(&id).unwrap().turn, Turn::Human);
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 4..=4), "two").unwrap();
    assert_eq!(svc.hand_over_round(&id).unwrap(), 2);
}

#[test]
fn taking_the_turn_back_returns_a_round_no_agent_received_to_the_drafts() {
    let repo = agent_branch();
    let svc = Service::new();
    let first = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = first.session().clone();
    drop(first);
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 1..=1), "one").unwrap();
    svc.hand_over_round(&id).unwrap();

    svc.take_turn_back(&id).unwrap();

    let session = svc.session(&id).unwrap();
    assert_eq!(session.round, 1, "Round 1 is being drafted again");
    assert!(session.threads[0].comments[0].is_draft());
    let next = svc.review(ReviewRequest::new(repo.path())).unwrap();
    assert_eq!(next.wait_timeout(Duration::from_millis(50)), None, "nothing is queued");
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 4..=4), "two").unwrap();
    svc.hand_over_round(&id).unwrap();
    let round1 = round(next.wait_timeout(Duration::from_secs(1)));
    assert_eq!(round1.number, 1);
    assert_eq!(round1.threads.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), vec!["T1", "T2"]);
}

#[test]
fn a_second_review_while_one_waits_on_the_session_is_refused() {
    let repo = agent_branch();
    let svc = Service::new();
    let first = svc.review(ReviewRequest::new(repo.path())).unwrap();

    let second = svc.review(ReviewRequest::new(repo.path()));

    assert!(matches!(second, Err(Error::Busy)), "got {:?}", second.as_ref().map(|p| p.session()));
    svc.approve(first.session()).unwrap();
    assert_eq!(first.wait_timeout(Duration::from_secs(1)), Some(Outcome::Approved), "the first still waits");
}

#[test]
fn a_review_call_that_went_away_frees_the_session_for_the_next() {
    let repo = agent_branch();
    let svc = Service::new();
    let first = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = first.session().clone();
    assert!(svc.session(&id).unwrap().agent_waiting);

    drop(first);
    assert!(!svc.session(&id).unwrap().agent_waiting);
    let second = svc.review(ReviewRequest::new(repo.path())).unwrap();

    svc.approve(&id).unwrap();
    assert_eq!(second.wait_timeout(Duration::from_secs(1)), Some(Outcome::Approved));
}

#[test]
fn a_round_received_but_never_acknowledged_goes_to_the_next_review() {
    let repo = agent_branch();
    let svc = Service::new();
    let first = svc.review(ReviewRequest::new(repo.path()).with_ack()).unwrap();
    let id = first.session().clone();
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 1..=1), "one").unwrap();
    svc.hand_over_round(&id).unwrap();

    assert_eq!(round(first.receive()).number, 1);
    drop(first); // say, killed before printing it

    let second = svc.review(ReviewRequest::new(repo.path()).with_ack()).unwrap();
    assert_eq!(round(second.receive()).number, 1);
    second.ack();
    drop(second);
    let third = svc.review(ReviewRequest::new(repo.path())).unwrap();
    assert_eq!(third.wait_timeout(Duration::from_millis(50)), None, "an acknowledged Round is delivered");
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
    svc.set_settings(Settings { untracked: false, ..Settings::default() });

    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();

    assert_eq!(paths(&svc, &pending), ["tracked.txt"]);
}

#[test]
fn a_review_call_overrides_the_untracked_setting() {
    let (repo, other) = (repo_with_untracked_and_ignored_files(), repo_with_untracked_and_ignored_files());
    let excluding = Service::new();
    excluding.set_settings(Settings { untracked: false, ..Settings::default() });
    let including = Service::new();

    let included = excluding.review(ReviewRequest::new(repo.path()).untracked(true)).unwrap();
    let excluded = including.review(ReviewRequest::new(other.path()).untracked(false)).unwrap();

    assert_eq!(paths(&excluding, &included), ["forgot.txt", "tracked.txt"]);
    assert_eq!(paths(&including, &excluded), ["tracked.txt"]);
}

/// Racy git: git trusts a file whose stat matches its index entry, unless the entry is no
/// older than the index file itself. An edit that keeps a file's size, in the same timestamp
/// tick as the commit before it (a whole second, for git built without nanosecond stats),
/// matches the index, and only that check catches it. Staged a minute in the past, so the
/// review can't land in a later tick by chance.
#[test]
fn a_same_size_edit_in_the_same_tick_as_the_index_write_is_reviewed() {
    let repo = TestRepo::new();
    // ctime can't be set from a test, so take it out of the stat comparison.
    repo.git(&["config", "core.trustctime", "false"]);
    let tick = std::time::SystemTime::now() - Duration::from_secs(60);
    let set_mtime = |file: &str| {
        let file = std::fs::File::options().write(true).open(repo.path().join(file)).unwrap();
        file.set_modified(tick).unwrap();
    };
    repo.write("tracked.txt", "a\n");
    set_mtime("tracked.txt");
    repo.commit_all("base");
    repo.write("tracked.txt", "b\n");
    set_mtime("tracked.txt");
    set_mtime(".git/index");
    let status = repo.git(&["--no-optional-locks", "status", "--porcelain"]);
    assert_eq!(status, "M tracked.txt", "git itself sees the edit");
    let svc = Service::new();

    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();

    assert_eq!(paths(&svc, &pending), ["tracked.txt"]);
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

#[test]
fn a_round_never_acknowledged_goes_to_the_review_already_waiting() {
    let repo = agent_branch();
    let svc = Service::new();
    let first = svc.review(ReviewRequest::new(repo.path()).with_ack()).unwrap();
    let id = first.session().clone();
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 1..=1), "one").unwrap();
    svc.hand_over_round(&id).unwrap();
    let second = svc.review(ReviewRequest::new(repo.path())).unwrap();

    drop(first);

    assert_eq!(round(second.wait_timeout(Duration::from_secs(1))).number, 1);
    assert_eq!(svc.session(&id).unwrap().turn, Turn::Agent);
}

/// A Session on `agent_branch` whose Round 1 handed the agent Thread T1, on new lines 2-3.
fn after_round_1(repo: &TestRepo, svc: &Service) -> SessionId {
    let first = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = first.session().clone();
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 2..=3), "Why rename?").unwrap();
    svc.hand_over_round(&id).unwrap();
    round(first.wait_timeout(Duration::from_secs(1)));
    id
}

#[test]
fn an_agent_reply_shows_in_its_thread_attributed_to_the_agent() {
    let repo = agent_branch();
    let svc = Service::new();
    let id = after_round_1(&repo, &svc);

    svc.reply(repo.path(), &ThreadId::new("T1"), "Renamed for clarity.").unwrap();

    let thread = &svc.session(&id).unwrap().threads[0];
    let comments: Vec<_> = thread.comments.iter().map(|c| (c.author.clone(), c.body.as_str(), c.round)).collect();
    assert_eq!(
        comments,
        vec![(Party::Human, "Why rename?", Some(1)), (Party::Agent, "Renamed for clarity.", Some(2))],
        "the reply comes back with Round 2, and isn't a Draft"
    );
}

#[test]
fn the_agent_can_only_answer_threads_it_has_received() {
    let repo = agent_branch();
    let svc = Service::new();
    let id = after_round_1(&repo, &svc);
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 4..=4), "Still a Draft").unwrap();

    let unknown = svc.reply(repo.path(), &ThreadId::new("T9"), "?").unwrap_err();
    let draft = svc.resolve(repo.path(), &ThreadId::new("T2")).unwrap_err();

    assert_eq!(unknown.to_string(), "no Thread T9 in this Session");
    assert_eq!(draft.to_string(), "no Thread T2 in this Session");
    assert!(svc.reply(repo.path(), &ThreadId::new("T1"), " \n").is_err(), "blank reply");
    let threads = svc.session(&id).unwrap().threads;
    assert_eq!(threads.iter().map(|t| t.comments.len()).collect::<Vec<_>>(), vec![1, 1]);
    assert!(threads.iter().all(|t| t.resolved.is_none()));
}

#[test]
fn replying_without_a_session_for_this_branch_fails() {
    let repo = agent_branch();
    let svc = Service::new();
    after_round_1(&repo, &svc);
    repo.branch("other");

    let error = svc.reply(repo.path(), &ThreadId::new("T1"), "Done.").unwrap_err();

    assert!(error.to_string().contains("no Session"), "{error}");
}

#[test]
fn the_agent_can_resolve_a_thread() {
    let repo = agent_branch();
    let svc = Service::new();
    let id = after_round_1(&repo, &svc);

    svc.resolve(repo.path(), &ThreadId::new("T1")).unwrap();

    assert_eq!(svc.session(&id).unwrap().threads[0].resolved, Some(Party::Agent));
}

#[test]
fn the_human_can_reopen_a_thread_the_agent_resolved_and_resolve_any_thread() {
    let repo = agent_branch();
    let svc = Service::new();
    let id = after_round_1(&repo, &svc);
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 4..=4), "A Draft").unwrap();
    svc.resolve(repo.path(), &ThreadId::new("T1")).unwrap();

    svc.reopen_thread(&ThreadRef::new(&id, &ThreadId::new("T1"))).unwrap();
    assert_eq!(svc.session(&id).unwrap().threads[0].resolved, None);

    svc.resolve_thread(&ThreadRef::new(&id, &ThreadId::new("T1"))).unwrap();
    svc.resolve_thread(&ThreadRef::new(&id, &ThreadId::new("T2"))).unwrap();
    let resolved: Vec<_> = svc.session(&id).unwrap().threads.iter().map(|t| t.resolved.clone()).collect();
    assert_eq!(resolved, vec![Some(Party::Human), Some(Party::Human)]);
    assert!(svc.resolve_thread(&ThreadRef::new(&id, &ThreadId::new("T9"))).is_err());
}

#[test]
fn a_human_reply_in_a_thread_is_a_draft_that_goes_out_with_the_next_round() {
    let repo = agent_branch();
    let svc = Service::new();
    let id = after_round_1(&repo, &svc);
    let t1 = ThreadId::new("T1");
    svc.reply(repo.path(), &t1, "Renamed for clarity.").unwrap();
    let second = svc.review(ReviewRequest::new(repo.path())).unwrap();

    svc.reply_in_thread(&ThreadRef::new(&id, &t1), "Clearer how?").unwrap();

    let thread = svc.session(&id).unwrap().threads.remove(0);
    let last = thread.comments.last().unwrap();
    assert_eq!((last.author.clone(), last.body.as_str(), last.is_draft()), (Party::Human, "Clearer how?", true));
    assert_eq!(second.wait_timeout(Duration::from_millis(50)), None, "a Draft doesn't reach the agent");

    svc.hand_over_round(&id).unwrap();

    let round2 = round(second.wait_timeout(Duration::from_secs(1)));
    assert_eq!(round2.threads.len(), 1);
    assert_eq!(round2.threads[0].id, t1);
    let sent: Vec<_> =
        round2.threads[0].comments.iter().map(|c| (c.author.clone(), c.body.as_str(), c.round)).collect();
    assert_eq!(sent, vec![(Party::Human, "Clearer how?", Some(2))], "only what this Round carries");
}

#[test]
fn a_human_reply_needs_text_and_a_known_thread() {
    let repo = agent_branch();
    let svc = Service::new();
    let id = after_round_1(&repo, &svc);

    assert!(svc.reply_in_thread(&ThreadRef::new(&id, &ThreadId::new("T1")), " \n").is_err());
    assert_eq!(
        svc.reply_in_thread(&ThreadRef::new(&id, &ThreadId::new("T9")), "?").unwrap_err().to_string(),
        "no Thread T9 in this Session"
    );
    assert_eq!(svc.session(&id).unwrap().threads[0].comments.len(), 1);
}

#[test]
fn a_thread_the_human_reopened_goes_out_with_the_next_round_marked_reopened() {
    let repo = agent_branch();
    let svc = Service::new();
    let id = after_round_1(&repo, &svc);
    let t1 = ThreadId::new("T1");
    svc.resolve(repo.path(), &t1).unwrap();
    let second = svc.review(ReviewRequest::new(repo.path())).unwrap();

    svc.reopen_thread(&ThreadRef::new(&id, &t1)).unwrap();
    assert!(svc.session(&id).unwrap().threads[0].reopened);
    svc.hand_over_round(&id).unwrap();

    let round2 = round(second.wait_timeout(Duration::from_secs(1)));
    assert_eq!(round2.threads.iter().map(|t| (t.id.as_str(), t.reopened)).collect::<Vec<_>>(), vec![("T1", true)]);
    assert!(round2.threads[0].comments.is_empty(), "no new Comment");
    let thread = svc.session(&id).unwrap().threads.remove(0);
    assert!(!thread.reopened, "handed over");
    assert_eq!(thread.resolved, None);
}

#[test]
fn taking_back_a_round_that_held_only_a_reopening_keeps_the_thread_reopened() {
    let repo = agent_branch();
    let svc = Service::new();
    let id = after_round_1(&repo, &svc);
    let t1 = ThreadId::new("T1");
    svc.resolve(repo.path(), &t1).unwrap();
    // The agent's next call went away: Round 2 waits for one.
    drop(svc.review(ReviewRequest::new(repo.path())).unwrap());
    svc.reopen_thread(&ThreadRef::new(&id, &t1)).unwrap();
    svc.hand_over_round(&id).unwrap();

    svc.take_turn_back(&id).unwrap();

    assert!(svc.session(&id).unwrap().threads[0].reopened, "withdrawn: the agent still has to be told");
    assert_eq!(svc.session(&id).unwrap().to_hand_over(), 1);
    svc.hand_over_round(&id).unwrap();
    let next = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let round2 = round(next.wait_timeout(Duration::from_secs(1)));
    assert_eq!(round2.threads.iter().map(|t| (t.id.as_str(), t.reopened)).collect::<Vec<_>>(), vec![("T1", true)]);
}

#[test]
fn a_thread_reopened_then_resolved_again_before_handover_does_not_go_out() {
    let repo = agent_branch();
    let svc = Service::new();
    let id = after_round_1(&repo, &svc);
    let t1 = ThreadId::new("T1");
    svc.resolve(repo.path(), &t1).unwrap();

    svc.reopen_thread(&ThreadRef::new(&id, &t1)).unwrap();
    svc.resolve_thread(&ThreadRef::new(&id, &t1)).unwrap();

    assert!(svc.hand_over_round(&id).is_err(), "nothing to hand over");
}

#[test]
fn reopening_an_open_thread_changes_nothing() {
    let repo = agent_branch();
    let svc = Service::new();
    let id = after_round_1(&repo, &svc);

    svc.reopen_thread(&ThreadRef::new(&id, &ThreadId::new("T1"))).unwrap();

    assert!(!svc.session(&id).unwrap().threads[0].reopened);
    assert!(svc.hand_over_round(&id).is_err(), "nothing to hand over");
}

#[test]
fn an_outdated_thread_can_be_resolved_by_either_party_and_reopened() {
    let repo = agent_branch();
    let svc = Service::new();
    let id = after_round_1(&repo, &svc);
    repo.write("lib.rs", "fn a() {}\nfn everything_else() {}\n");
    svc.review(ReviewRequest::new(repo.path())).unwrap();
    assert!(svc.session(&id).unwrap().threads[0].outdated);
    let t1 = ThreadId::new("T1");
    let state = || {
        let t = svc.session(&id).unwrap().threads.remove(0);
        (t.outdated, t.resolved)
    };

    svc.resolve(repo.path(), &t1).unwrap();
    assert_eq!(state(), (true, Some(Party::Agent)));
    svc.reopen_thread(&ThreadRef::new(&id, &t1)).unwrap();
    assert_eq!(state(), (true, None));
    svc.resolve_thread(&ThreadRef::new(&id, &t1)).unwrap();
    assert_eq!(state(), (true, Some(Party::Human)));
}

#[test]
fn a_handed_over_round_carries_whether_each_thread_is_resolved() {
    let repo = agent_branch();
    let svc = Service::new();
    let id = after_round_1(&repo, &svc);
    let second = svc.review(ReviewRequest::new(repo.path())).unwrap();
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 4..=4), "Add a test for d").unwrap();
    svc.resolve_thread(&ThreadRef::new(&id, &ThreadId::new("T2"))).unwrap();

    svc.hand_over_round(&id).unwrap();

    let round2 = round(second.wait_timeout(Duration::from_secs(1)));
    assert_eq!(
        round2.threads.iter().map(|t| (t.id.as_str(), t.resolved.clone())).collect::<Vec<_>>(),
        vec![("T2", Some(Party::Human))]
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

#[test]
fn since_the_last_round_threads_on_the_old_side_are_not_placed_in_the_diff() {
    let svc = Service::new();
    let (_repo, second) = second_round(&svc);
    let id = second.session();
    // On the base's code, which the old side since the last Round doesn't show.
    svc.show(id, ChangesetView::Full).unwrap();
    svc.comment(id, Anchor::lines(Side::Old, "lib.rs", 1..=1), "Why keep a?").unwrap();
    svc.show(id, ChangesetView::SinceLastRound).unwrap();

    let placed = svc.session(id).unwrap().placed_threads();

    assert_eq!(placed, vec![PlacedThread { file: 1, side: Side::New, line: 2, thread: ThreadId::new("T1") }]);
    svc.show(id, ChangesetView::Full).unwrap();
    let placed = svc.session(id).unwrap().placed_threads();
    assert_eq!(placed.iter().map(|p| (p.side, p.line)).collect::<Vec<_>>(), [(Side::New, 2), (Side::Old, 1)]);
}

#[test]
fn a_selection_anchors_to_its_lines_or_across_sides_to_its_last_line() {
    let repo = agent_branch();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let session = svc.session(pending.session()).unwrap();

    let new_lines = session.anchor_for_selection(0, (Side::New, 3), (Side::New, 2));
    let across = session.anchor_for_selection(0, (Side::Old, 2), (Side::New, 4));

    assert_eq!(new_lines, Some(Anchor::lines(Side::New, "lib.rs", 2..=3)));
    assert_eq!(across, Some(Anchor::lines(Side::New, "lib.rs", 4..=4)));
    assert_eq!(session.anchor_for_selection(5, (Side::New, 1), (Side::New, 1)), None, "no such file");
}

#[test]
fn since_the_last_round_old_side_lines_take_no_comment() {
    let svc = Service::new();
    let (_repo, second) = second_round(&svc);
    let session = svc.session(second.session()).unwrap();

    // That old side is the last Round's code, which a Thread can't anchor to.
    assert_eq!(session.anchor_for_selection(1, (Side::Old, 1), (Side::Old, 1)), None);
    assert_eq!(
        session.anchor_for_selection(1, (Side::New, 3), (Side::New, 4)),
        Some(Anchor::lines(Side::New, "lib.rs", 3..=4))
    );
}

#[test]
fn a_session_counts_its_drafts() {
    let svc = Service::new();
    let (_repo, second) = second_round(&svc);
    let id = second.session();
    svc.comment(id, Anchor::file("extra.rs"), "Why a new file?").unwrap();
    svc.reply_in_thread(&ThreadRef::new(id, &ThreadId::new("T1")), "And d_works?").unwrap();

    // T1's opening Comment went out with Round 1.
    assert_eq!(svc.session(id).unwrap().drafts(), 2);
}

#[test]
fn a_local_session_is_titled_by_its_repository_and_branch() {
    let repo = agent_branch();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();

    let title = svc.session(pending.session()).unwrap().title();

    let name = repo.path().file_name().unwrap().to_string_lossy();
    assert_eq!(title, format!("{name} · agent"));
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
    let mut before = svc.sessions();
    assert!(
        before[0].last_round.is_some() && !before[0].viewed.is_empty() && before[1].attention,
        "the Sessions to restore lack a Round, a tick or attention: {before:?}"
    );
    drop(svc);

    let restored = Service::open(dirs.state()).unwrap();

    // No `review` call survives a restart.
    before.iter_mut().for_each(|s| s.agent_waiting = false);
    assert_eq!(restored.sessions(), before);
}

#[test]
fn sessions_saved_with_a_repo_branch_and_pull_request_still_load_with_their_source() {
    let dirs = TestDirs::new();
    let sessions = dirs.state().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    // As saved before a Session had a Source: local work with its repo and branch, a Pull
    // Request with an empty repo.
    let local = r#"{"view":{"id":"s1","repo":"/work/app","branch":"feature","pull_request":null,"round":1}}"#;
    let pr = r#"{"view":{"id":"s2","repo":"","branch":"sso","pull_request":{"id":{"host":"github.com",
        "repo":"acme/web","number":482},"url":"https://github.com/acme/web/pull/482","title":"Add SSO login",
        "author":"maria","head_branch":"sso","head_sha":"head1","base_sha":"base1"},"round":1}}"#;
    std::fs::write(sessions.join("s1.json"), local).unwrap();
    std::fs::write(sessions.join("s2.json"), pr).unwrap();

    let svc = Service::open(dirs.state()).unwrap();

    let s1 = svc.session(&SessionId::new("s1")).unwrap();
    assert_eq!(s1.source, Source::Local { repo: "/work/app".into(), branch: "feature".into() });
    let s2 = svc.session(&SessionId::new("s2")).unwrap();
    let Source::PullRequest(pull_request) = &s2.source else { panic!("a Pull Request Session: {:?}", s2.source) };
    assert_eq!((pull_request.url.as_str(), s2.branch()), ("https://github.com/acme/web/pull/482", "sso"));
}

#[test]
fn replies_resolutions_and_reopenings_survive_a_restart_and_still_go_out() {
    let dirs = TestDirs::new();
    let repo = agent_branch();
    let svc = Service::open(dirs.state()).unwrap();
    let id = session_mid_review(&svc, &repo);
    let t1 = ThreadId::new("T1");
    svc.reply(repo.path(), &t1, "Renamed for clarity.").unwrap();
    svc.resolve(repo.path(), &t1).unwrap();
    svc.reopen_thread(&ThreadRef::new(&id, &t1)).unwrap();
    svc.reply_in_thread(&ThreadRef::new(&id, &t1), "Clearer how?").unwrap();
    let before = svc.session(&id).unwrap();
    drop(svc);

    let restored = Service::open(dirs.state()).unwrap();
    assert_eq!(restored.session(&id).unwrap().threads, before.threads);
    let next = restored.review(ReviewRequest::new(repo.path())).unwrap();
    restored.hand_over_round(&id).unwrap();

    let round2 = round(next.wait_timeout(Duration::from_secs(1)));
    let t1_sent = round2.threads.iter().find(|t| t.id == t1).expect("T1 goes out");
    assert!(t1_sent.reopened);
    assert_eq!(t1_sent.comments.iter().map(|c| c.body.as_str()).collect::<Vec<_>>(), vec!["Clearer how?"]);
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
    svc.take_turn_back(&id).unwrap();
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
    let (approved_repo, abandoned_repo, open) = (agent_branch(), agent_branch(), agent_branch());
    let svc = Service::open(dirs.state()).unwrap();
    let approved = session_mid_review(&svc, &approved_repo);
    let abandoned = session_mid_review(&svc, &abandoned_repo);
    let open = session_mid_review(&svc, &open);
    // Both agents wait, so they collect the outcome at once.
    let _approved_call = svc.review(ReviewRequest::new(approved_repo.path())).unwrap();
    let _abandoned_call = svc.review(ReviewRequest::new(abandoned_repo.path())).unwrap();
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

#[test]
fn a_verdict_given_while_no_review_waits_goes_to_the_next_review_which_ends_the_session() {
    let repo = agent_branch();
    let svc = Service::new();
    let first = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = first.session().clone();
    drop(first);

    svc.approve(&id).unwrap();
    assert_eq!(svc.session(&id).unwrap().ended, Some(Outcome::Approved), "kept until the agent collects it");
    assert!(svc.abandon(&id).is_err(), "the Session already has its outcome");

    let next = svc.review(ReviewRequest::new(repo.path())).unwrap();
    assert_eq!(next.wait_timeout(Duration::from_secs(1)), Some(Outcome::Approved));
    assert!(svc.session(&id).is_none());
}

#[test]
fn the_human_can_close_a_session_whose_verdict_no_agent_collected() {
    let dirs = TestDirs::new();
    let repo = agent_branch();
    let svc = Service::open(dirs.state()).unwrap();
    let first = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = first.session().clone();
    drop(first);
    svc.approve(&id).unwrap();

    svc.close(&id).unwrap();

    assert!(svc.session(&id).is_none());
    drop(svc);
    let restored = Service::open(dirs.state()).unwrap();
    assert!(restored.sessions().is_empty(), "closing removes it from what's saved too");
    // The Verdict went with it: the agent's next `review` starts a new Session.
    let next = restored.review(ReviewRequest::new(repo.path())).unwrap();
    assert!(next.wait_timeout(Duration::from_millis(50)).is_none(), "waits for the human");
    assert_eq!(restored.session(next.session()).unwrap().round, 1);
}

#[test]
fn the_human_can_close_a_session_abandoned_while_no_review_waited() {
    let repo = agent_branch();
    let svc = Service::new();
    let first = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = first.session().clone();
    drop(first);
    svc.abandon(&id).unwrap();

    svc.close(&id).unwrap();

    assert!(svc.sessions().is_empty());
}

#[test]
fn closing_local_work_without_a_verdict_is_refused() {
    let repo = agent_branch();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();

    assert!(
        matches!(svc.close(pending.session()), Err(Error::InvalidRequest(_))),
        "closing local work without a Verdict wasn't refused"
    );
}

#[test]
fn abandoning_while_no_review_waits_tells_the_next_review() {
    let repo = agent_branch();
    let svc = Service::new();
    let first = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = first.session().clone();
    drop(first);

    svc.abandon(&id).unwrap();

    let next = svc.review(ReviewRequest::new(repo.path())).unwrap();
    assert_eq!(next.wait_timeout(Duration::from_secs(1)), Some(Outcome::Abandoned));
    assert!(svc.session(&id).is_none());
}

#[test]
fn a_verdict_received_but_never_acknowledged_goes_to_the_next_review() {
    let repo = agent_branch();
    let svc = Service::new();
    let first = svc.review(ReviewRequest::new(repo.path()).with_ack()).unwrap();
    let id = first.session().clone();
    svc.approve(&id).unwrap();

    assert_eq!(first.receive(), Some(Outcome::Approved));
    drop(first); // killed before printing it
    assert!(svc.session(&id).is_some(), "not delivered yet");

    let second = svc.review(ReviewRequest::new(repo.path()).with_ack()).unwrap();
    assert_eq!(second.receive(), Some(Outcome::Approved));
    second.ack();
    assert!(svc.session(&id).is_none());
}

#[test]
fn approving_during_the_agents_turn_is_refused_but_abandoning_is_not() {
    let repo = agent_branch();
    let svc = Service::new();
    let first = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = first.session().clone();
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 1..=1), "one").unwrap();
    svc.hand_over_round(&id).unwrap();
    round(first.wait_timeout(Duration::from_secs(1)));

    assert!(matches!(svc.approve(&id), Err(Error::NotYourTurn)), "approving in the agent's Turn wasn't refused");
    assert!(svc.session(&id).is_some(), "the refused approval ended the Session");

    svc.abandon(&id).unwrap();
}

#[test]
fn withdrawing_a_round_restores_the_last_round_it_replaced() {
    let repo = agent_branch();
    let svc = Service::new();
    let first = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = first.session().clone();
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 1..=1), "one").unwrap();
    svc.hand_over_round(&id).unwrap();
    round(first.wait_timeout(Duration::from_secs(1)));
    let round1 = svc.session(&id).unwrap().last_round;
    repo.write("lib.rs", "fn a() {}\nfn b2() {}\nfn c2() {}\nfn d() {}\nfn e() {}\n");
    let second = svc.review(ReviewRequest::new(repo.path())).unwrap();
    drop(second);
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 5..=5), "two").unwrap();
    svc.hand_over_round(&id).unwrap();
    assert_ne!(svc.session(&id).unwrap().last_round, round1);

    svc.take_turn_back(&id).unwrap();

    let session = svc.session(&id).unwrap();
    assert_eq!(session.round, 2);
    assert_eq!(session.last_round, round1);
    let since = session.since_last_round.expect("Round 1 is still the last Round");
    assert_eq!(since.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["lib.rs"]);
    assert_eq!(since.files[0].old, round1.unwrap().files[0].new, "compared against Round 1's code");
}

#[test]
fn a_verdict_no_agent_collected_before_a_restart_goes_to_the_next_review() {
    let dirs = TestDirs::new();
    let repo = agent_branch();
    let svc = Service::open(dirs.state()).unwrap();
    let first = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = first.session().clone();
    drop(first);
    svc.approve(&id).unwrap();
    drop(svc);

    let restored = Service::open(dirs.state()).unwrap();
    let next = restored.review(ReviewRequest::new(repo.path())).unwrap();

    assert_eq!(next.wait_timeout(Duration::from_secs(1)), Some(Outcome::Approved));
    assert!(restored.sessions().is_empty());
}
