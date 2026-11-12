//! Seam A: File and Review Anchors, and code suggestions, through the core review service
//! against real throwaway git repositories.

use std::time::Duration;

use akshually_core::{Anchor, NewComment, Outcome, ReviewRequest, Service, Side};
use akshually_testkit::TestRepo;
use serde_json::json;

/// A repository on branch `agent` whose `lib.rs` changed two lines and gained a third.
fn agent_branch() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("lib.rs", "fn a() {}\nfn b() {}\nfn c() {}\n").commit_all("base");
    repo.branch("agent").write("lib.rs", "fn a() {}\nfn b2() {}\nfn c2() {}\nfn d() {}\n");
    repo
}

#[test]
fn a_comment_can_anchor_to_a_whole_changed_file() {
    let repo = agent_branch();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();

    let thread = svc.comment(pending.session(), Anchor::file("lib.rs"), "Split this file?").unwrap();

    let session = svc.session(pending.session()).unwrap();
    let t = &session.threads[0];
    assert_eq!(t.id, thread);
    assert_eq!(t.anchor, Anchor::file("lib.rs"));
    assert_eq!(t.excerpt, "", "a file Anchor holds no lines");
    assert_eq!(t.comments[0].body, "Split this file?");
    assert!(svc.comment(pending.session(), Anchor::file("other.rs"), "x").is_err(), "unchanged file");
}

#[test]
fn a_comment_can_anchor_to_the_review_as_a_whole() {
    let repo = agent_branch();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();

    svc.comment(pending.session(), Anchor::Review, "Nice work overall.").unwrap();

    let t = &svc.session(pending.session()).unwrap().threads[0];
    assert_eq!(t.anchor, Anchor::Review);
    assert_eq!(t.excerpt, "");
}

#[test]
fn a_comment_can_suggest_replacement_code_for_its_new_lines() {
    let repo = agent_branch();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = pending.session().clone();
    let anchor = Anchor::lines(Side::New, "lib.rs", 2..=3);

    svc.comment(&id, anchor, NewComment::new("Keep the old names.").suggest("fn b() {}\nfn c() {}\n")).unwrap();
    svc.hand_over_round(&id).unwrap();

    let Some(Outcome::Round(round)) = pending.wait_timeout(Duration::from_secs(1)) else { panic!("no Round") };
    let comment = &round.threads[0].comments[0];
    assert_eq!(comment.body, "Keep the old names.");
    assert_eq!(comment.suggestion.as_deref(), Some("fn b() {}\nfn c() {}\n"));
    assert_eq!(round.threads[0].excerpt, "fn b2() {}\nfn c2() {}\n", "the code it replaces");
}

/// The composer takes a suggestion as GitHub does: a ```` ```suggestion ```` fence in the body.
#[test]
fn a_suggestion_fence_in_written_markdown_becomes_the_suggestion() {
    let repo = agent_branch();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let written = "Keep the old names:\n\n```suggestion\nfn b() {}\nfn c() {}\n```\n\nThey're used elsewhere.\n";

    svc.comment(pending.session(), Anchor::lines(Side::New, "lib.rs", 2..=3), NewComment::from_markdown(written))
        .unwrap();

    let comment = &svc.session(pending.session()).unwrap().threads[0].comments[0];
    assert_eq!(comment.body, "Keep the old names:\n\nThey're used elsewhere.\n");
    assert_eq!(comment.suggestion.as_deref(), Some("fn b() {}\nfn c() {}\n"));
}

#[test]
fn markdown_without_a_suggestion_fence_is_kept_whole() {
    let written = "Use this:\n\n```rust\nfn b() {}\n```\n";

    assert_eq!(NewComment::from_markdown(written), NewComment::new(written));
}

#[test]
fn a_suggestion_needs_lines_on_the_new_side() {
    let repo = agent_branch();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = pending.session();
    let suggestion = || NewComment::new("x").suggest("y\n");

    assert!(svc.comment(id, Anchor::lines(Side::Old, "lib.rs", 2..=2), suggestion()).is_err(), "old side");
    assert!(svc.comment(id, Anchor::file("lib.rs"), suggestion()).is_err(), "whole file");
    assert!(svc.comment(id, Anchor::Review, suggestion()).is_err(), "whole Review");
    assert!(svc.session(id).unwrap().threads.is_empty());
}

#[test]
fn a_plain_comment_carries_no_suggestion() {
    let repo = agent_branch();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();

    svc.comment(pending.session(), Anchor::lines(Side::New, "lib.rs", 2..=2), "Why b2?").unwrap();

    assert_eq!(svc.session(pending.session()).unwrap().threads[0].comments[0].suggestion, None);
}

/// The Round's JSON is what the channel carries and `review --format json` prints.
#[test]
fn round_json_names_each_anchor_kind_and_carries_suggestions() {
    let repo = agent_branch();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = pending.session().clone();
    let suggestion = NewComment::new("Rename back.").suggest("fn b() {}\n");
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 2..=2), suggestion).unwrap();
    svc.comment(&id, Anchor::file("lib.rs"), "Split this file.").unwrap();
    svc.comment(&id, Anchor::Review, "Nice work overall.").unwrap();
    svc.hand_over_round(&id).unwrap();

    let Some(Outcome::Round(round)) = pending.wait_timeout(Duration::from_secs(1)) else { panic!("no Round") };
    let json = serde_json::to_value(&round).unwrap();

    let threads = json["threads"].as_array().unwrap();
    assert_eq!(threads[0]["anchor"], json!({"kind": "lines", "path": "lib.rs", "side": "new", "start": 2, "end": 2}));
    assert_eq!(threads[0]["comments"][0]["suggestion"], "fn b() {}\n");
    assert_eq!(threads[1]["anchor"], json!({"kind": "file", "path": "lib.rs"}));
    assert_eq!(threads[1]["comments"][0].get("suggestion"), None, "no suggestion, no field");
    assert_eq!(threads[2]["anchor"], json!({"kind": "review"}));
}

#[test]
fn file_and_review_threads_are_handed_over_like_line_threads() {
    let repo = agent_branch();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let id = pending.session().clone();
    svc.comment(&id, Anchor::lines(Side::New, "lib.rs", 2..=2), "Why b2?").unwrap();
    svc.comment(&id, Anchor::file("lib.rs"), "Split this file.").unwrap();
    svc.comment(&id, Anchor::Review, "Nice work overall.").unwrap();

    svc.hand_over_round(&id).unwrap();

    let Some(Outcome::Round(round)) = pending.wait_timeout(Duration::from_secs(1)) else { panic!("no Round") };
    let sent: Vec<_> = round.threads.iter().map(|t| (t.id.as_str(), &t.anchor, t.comments[0].body.as_str())).collect();
    assert_eq!(
        sent,
        vec![
            ("T1", &Anchor::lines(Side::New, "lib.rs", 2..=2), "Why b2?"),
            ("T2", &Anchor::file("lib.rs"), "Split this file."),
            ("T3", &Anchor::Review, "Nice work overall."),
        ]
    );
}
