//! Seam A: File and Review Anchors, and code suggestions, through the core review service
//! against real throwaway git repositories; and how an Anchor reads, wherever it is shown.

use std::time::Duration;

use akshually_core::{Anchor, Error, NewComment, Outcome, ReviewRequest, Service, Side};
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
fn an_anchor_reads_as_its_file_side_and_lines_or_as_a_whole_file_or_review() {
    let cases = [
        (Anchor::lines(Side::New, "lib.rs", 3..=3), "lib.rs, new line 3"),
        (Anchor::lines(Side::New, "lib.rs", 2..=3), "lib.rs, new lines 2-3"),
        (Anchor::lines(Side::Old, "lib.rs", 1..=1), "lib.rs, old line 1"),
        (Anchor::lines(Side::Old, "lib.rs", 1..=2), "lib.rs, old lines 1-2"),
        (Anchor::file("lib.rs"), "lib.rs, whole file"),
        (Anchor::Review, "whole Review"),
    ];

    for (anchor, expected) in cases {
        assert_eq!(anchor.to_string(), expected, "{anchor:?}");
    }
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
fn a_comment_with_a_suggestion_is_written_as_markdown_that_reads_back_the_same() {
    let comment = NewComment::new("Name it.").suggest("    okta();\n");

    let written = comment.to_markdown();

    assert_eq!(written, "Name it.\n\n```suggestion\n    okta();\n```\n");
    assert_eq!(NewComment::from_markdown(&written), comment);
}

#[test]
fn a_suggestion_holding_a_fence_is_written_inside_a_longer_one() {
    let comment = NewComment::new("Quote it.").suggest("/// ```\n/// okta();\n/// ```\n");

    let written = comment.to_markdown();

    assert!(written.contains("\n````suggestion\n"), "{written}");
    assert_eq!(NewComment::from_markdown(&written), comment);
}

#[test]
fn a_comment_without_a_suggestion_is_written_as_its_body() {
    let comment = NewComment::new("Use this:\n\n```rust\nfn b() {}\n```\n");

    let written = comment.to_markdown();

    assert_eq!(NewComment::from_markdown(&written), comment);
}

#[test]
fn checking_an_anchor_refuses_a_file_outside_the_changeset() {
    let repo = agent_branch();
    let svc = Service::new();
    let pending = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let session = svc.session(pending.session()).unwrap();

    let (changed, unchanged) =
        (session.check_anchor(&Anchor::file("lib.rs")), session.check_anchor(&Anchor::file("other.rs")));

    assert!(changed.is_ok(), "{changed:?}");
    assert!(matches!(unchanged, Err(Error::InvalidAnchor(_))), "{unchanged:?}");
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
