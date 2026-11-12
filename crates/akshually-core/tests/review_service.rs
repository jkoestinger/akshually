//! Seam A: the core review service, in-process, against real throwaway git repositories.

use std::time::Duration;

use akshually_core::{Anchor, FileChange, Outcome, Party, ReviewRequest, Round, Service, SessionId, Side};
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

    repo.write("lib.rs", "fn a() {}\nfn b() {}\nfn c2() {}\nfn d() {}\n");
    let second = svc.review(ReviewRequest::new(repo.path())).unwrap();

    assert_eq!(second.session(), &id);
    assert_eq!(svc.sessions().len(), 1);
    let session = svc.session(&id).unwrap();
    assert_eq!(session.changeset.files[0].new.as_deref(), Some("fn a() {}\nfn b() {}\nfn c2() {}\nfn d() {}\n"));
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
    session_mid_review(&svc, &repo);
    let _waiting = svc.review(ReviewRequest::new(other.path())).unwrap();
    let before = svc.sessions();
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
