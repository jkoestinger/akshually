//! Seam A, the core review service: local files changing on disk, and reloading.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use akshually_core::{Anchor, ChangesetView, ReviewRequest, Service, Side, ThreadId, Turn};
use akshually_testkit::{TestDirs, TestRepo, wait_until};

/// A repository on branch `agent` whose `lib.rs` has uncommitted changes.
fn agent_branch() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("lib.rs", "fn a() {}\nfn b() {}\nfn c() {}\n").write("other.rs", "x\n").commit_all("base");
    repo.branch("agent").write("lib.rs", "fn a() {}\nfn b2() {}\nfn c2() {}\n");
    repo
}

fn paths(paths: &[&str]) -> BTreeSet<String> {
    paths.iter().map(|p| p.to_string()).collect()
}

#[test]
fn a_change_on_disk_during_the_humans_turn_is_offered_without_changing_the_diff() {
    let repo = agent_branch();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let before = svc.session(pending.session()).unwrap().changeset;

    repo.write("lib.rs", "fn a() {}\nfn b3() {}\n").write("other.rs", "y\n");
    svc.check_disk();

    let session = svc.session(pending.session()).unwrap();
    assert_eq!(session.changed_on_disk, paths(&["lib.rs", "other.rs"]));
    assert_eq!(session.changeset, before, "the displayed diff stays as it was");
}

#[test]
fn a_change_on_disk_during_the_agents_turn_is_not_offered() {
    let repo = agent_branch();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = pending.session().clone();
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 2..=2), "Why?").unwrap();
    svc.hand_over_round(&id).unwrap();

    repo.write("lib.rs", "fn a() {}\nfn b3() {}\n");
    svc.check_disk();

    let session = svc.session(&id).unwrap();
    assert_eq!(session.turn, Turn::Agent);
    assert!(session.changed_on_disk.is_empty());
}

#[test]
fn handing_over_the_round_withdraws_an_offered_reload() {
    let repo = agent_branch();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = pending.session().clone();
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 2..=2), "Why?").unwrap();
    repo.write("lib.rs", "fn a() {}\nfn b3() {}\n");
    svc.check_disk();

    svc.hand_over_round(&id).unwrap();

    assert!(svc.session(&id).unwrap().changed_on_disk.is_empty(), "the agent's next review refreshes anyway");
}

#[test]
fn reloading_shows_the_changes_on_disk_and_re_anchors_threads_and_drafts() {
    let repo = agent_branch();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = pending.session().clone();
    let moved = svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 2..=2), "Why b2?").unwrap();
    let gone = svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 3..=3), "Why c2?").unwrap();
    repo.write("lib.rs", "// header\nfn a() {}\nfn b2() {}\n");
    svc.check_disk();

    svc.reload(&id).unwrap();

    let session = svc.session(&id).unwrap();
    assert_eq!(session.changeset.files[0].new.as_deref(), Some("// header\nfn a() {}\nfn b2() {}\n"));
    assert!(session.changed_on_disk.is_empty(), "nothing left to reload");
    let thread = |t: &ThreadId| session.threads.iter().find(|thread| &thread.id == t).unwrap();
    assert_eq!(thread(&moved).anchor, Anchor::lines(Side::New, "lib.rs", 3..=3));
    assert!(!thread(&moved).outdated);
    assert!(thread(&gone).outdated);
    assert!(thread(&gone).comments[0].is_draft(), "Drafts stay Drafts");
}

#[test]
fn reloading_in_a_later_round_recomputes_the_changes_since_it_and_clears_ticks_on_changed_files() {
    let repo = agent_branch();
    let svc = Service::new();
    let id = svc.review(ReviewRequest::new(repo.path())).unwrap().session().clone();
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 2..=2), "Why?").unwrap();
    svc.hand_over_round(&id).unwrap();
    repo.write("other.rs", "y\n");
    let _round_2 = svc.review(ReviewRequest::new(repo.path())).unwrap();
    svc.mark_viewed(&id, "lib.rs", true).unwrap();
    svc.mark_viewed(&id, "other.rs", true).unwrap();
    svc.show(&id, ChangesetView::Full).unwrap();

    repo.write("lib.rs", "fn a() {}\nfn b2() {}\nfn c3() {}\n");
    svc.reload(&id).unwrap();

    let session = svc.session(&id).unwrap();
    let since: Vec<_> = session.since_last_round.unwrap().files.into_iter().map(|f| f.path).collect();
    assert_eq!(since, ["lib.rs", "other.rs"]);
    assert_eq!(session.viewed, paths(&["other.rs"]));
    assert_eq!(session.view, ChangesetView::Full, "the human's choice of view stays");
}

#[test]
fn undoing_the_change_on_disk_withdraws_the_offer() {
    let repo = agent_branch();
    let svc = Service::new();
    let id = svc.review(ReviewRequest::new(repo.path())).unwrap().session().clone();
    repo.write("other.rs", "y\n");
    svc.check_disk();

    repo.write("other.rs", "x\n");
    svc.check_disk();

    assert!(svc.session(&id).unwrap().changed_on_disk.is_empty());
}

#[test]
fn the_disk_watcher_offers_changes_without_being_asked() {
    let repo = agent_branch();
    let svc = Arc::new(Service::new());
    let id = svc.review(ReviewRequest::new(repo.path())).unwrap().session().clone();
    svc.watch_disk(Duration::from_millis(20));

    repo.write("other.rs", "y\n");

    wait_until(Duration::from_secs(10), "the change on disk to be offered", || {
        (svc.session(&id).unwrap().changed_on_disk == paths(&["other.rs"])).then_some(())
    });
}

#[test]
fn a_restored_session_reloads_with_what_the_agents_review_asked_for() {
    let repo = agent_branch();
    let dirs = TestDirs::new();
    let id = {
        let svc = Service::open(dirs.state()).unwrap();
        svc.review(ReviewRequest::new(repo.path()).untracked(false)).unwrap().session().clone()
    };
    let restored = Service::open(dirs.state()).unwrap();

    repo.write("untracked.rs", "new\n").write("other.rs", "y\n");
    restored.reload(&id).unwrap();

    let files: Vec<_> = restored.session(&id).unwrap().changeset.files.into_iter().map(|f| f.path).collect();
    assert_eq!(files, ["lib.rs", "other.rs"], "still without untracked files");
}

#[test]
fn a_change_after_checks_that_found_nothing_is_still_offered() {
    let repo = agent_branch();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    svc.check_disk();
    svc.check_disk();
    assert!(svc.session(pending.session()).unwrap().changed_on_disk.is_empty());

    repo.write("other.rs", "y\n");
    svc.check_disk();

    assert_eq!(svc.session(pending.session()).unwrap().changed_on_disk, paths(&["other.rs"]));
}

#[test]
fn going_back_on_disk_to_what_was_checked_before_a_reload_is_offered() {
    let repo = agent_branch();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = pending.session().clone();
    svc.check_disk();
    repo.write("other.rs", "y\n");
    svc.reload(&id).unwrap();

    repo.write("other.rs", "x\n");
    svc.check_disk();

    assert_eq!(svc.session(&id).unwrap().changed_on_disk, paths(&["other.rs"]), "the diff shows y");
}

#[test]
fn reloading_a_changed_changeset_moves_its_revision() {
    let repo = agent_branch();
    let svc = Service::new();
    let id = svc.review(ReviewRequest::new(repo.path())).unwrap().session().clone();
    let before = svc.session(&id).unwrap().changeset_rev;

    repo.write("lib.rs", "fn a() {}\nfn b3() {}\n");
    svc.reload(&id).unwrap();

    assert_ne!(svc.session(&id).unwrap().changeset_rev, before);
}

#[test]
fn the_agents_next_review_refreshing_the_changeset_moves_its_revision() {
    let repo = agent_branch();
    let svc = Service::new();
    let id = svc.review(ReviewRequest::new(repo.path())).unwrap().session().clone();
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 2..=2), "Why?").unwrap();
    svc.hand_over_round(&id).unwrap();
    let before = svc.session(&id).unwrap().changeset_rev;

    repo.write("lib.rs", "fn a() {}\nfn b3() {}\n");
    let _round_2 = svc.review(ReviewRequest::new(repo.path())).unwrap();

    assert_ne!(svc.session(&id).unwrap().changeset_rev, before);
}

#[test]
fn switching_the_changeset_view_moves_its_revision() {
    let repo = agent_branch();
    let svc = Service::new();
    let id = svc.review(ReviewRequest::new(repo.path())).unwrap().session().clone();
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 2..=2), "Why?").unwrap();
    svc.hand_over_round(&id).unwrap();
    repo.write("other.rs", "y\n");
    let _round_2 = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let before = svc.session(&id).unwrap().changeset_rev;

    svc.show(&id, ChangesetView::Full).unwrap();

    assert_ne!(svc.session(&id).unwrap().changeset_rev, before);
}

#[test]
fn what_leaves_the_changeset_shown_alone_leaves_its_revision_put() {
    let repo = agent_branch();
    let svc = Service::new();
    let id = svc.review(ReviewRequest::new(repo.path())).unwrap().session().clone();
    let before = svc.session(&id).unwrap().changeset_rev;

    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 2..=2), "Why?").unwrap();
    svc.mark_viewed(&id, "lib.rs", true).unwrap();
    svc.show(&id, ChangesetView::Full).unwrap();
    repo.write("other.rs", "y\n");
    svc.check_disk();
    svc.display_inbox();

    assert_eq!(svc.session(&id).unwrap().changeset_rev, before);
}

#[test]
fn reloading_an_unchanged_changeset_leaves_its_revision_put() {
    let repo = agent_branch();
    let svc = Service::new();
    let id = svc.review(ReviewRequest::new(repo.path())).unwrap().session().clone();
    let before = svc.session(&id).unwrap().changeset_rev;

    svc.reload(&id).unwrap();

    assert_eq!(svc.session(&id).unwrap().changeset_rev, before);
}
