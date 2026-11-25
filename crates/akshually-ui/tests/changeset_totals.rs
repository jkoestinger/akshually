//! UI: the totals of the Changeset shown (files, lines added, lines removed) in a Session's
//! header, in Rune's headless `Runtime` against real Sessions in the core service (#95).

use std::sync::Arc;
use std::time::Duration;

use rune::{Env, Runtime};

use akshually_core::forge::{ForgeAdapter, ForgeError, PullRequest, PullRequestRef, RemoteThread, Review};
use akshually_core::{Anchor, Changeset, ChangesetView, FileChange, ReviewRequest, Service, Side, Verdict};
use akshually_testkit::TestRepo;
use akshually_ui::{State, view};

/// Lines enough for the core to call a file Large (it allows 2,000 changed lines).
const LARGE: usize = 2_001;

/// A headless window over `service`, its first frame built.
fn window(service: &Arc<Service>) -> Runtime<State> {
    let mut rt = Runtime::new(State::new(service.clone()), view, (1280.0, 800.0), Env::default());
    rt.frame(0.0);
    rt
}

/// Hands the service's Window to the window, as its subscriber does, and builds the next frame.
fn deliver_window(rt: &mut Runtime<State>, service: &Service) {
    let window = service.window();
    rt.update_if(move |s: &mut State, _| {
        s.apply(window);
        true
    });
    rt.frame(0.0);
}

/// The texts drawn inside the element keyed `key`, in tree order; empty when nothing has the key.
fn texts_within(rt: &Runtime<State>, key: &str) -> Vec<String> {
    let elements = rt.elements();
    let inside = |mut at: usize| loop {
        if elements[at].key.as_deref() == Some(key) {
            return true;
        }
        match elements[at].parent {
            Some(parent) => at = parent,
            None => return false,
        }
    };
    (0..elements.len()).filter(|&at| inside(at)).filter_map(|at| elements[at].text.clone()).collect()
}

/// The text colour of the first element keyed `key` that shows `text`.
fn colour_of(rt: &Runtime<State>, key: &str, text: &str) -> rune::Color {
    let found = rt.elements().into_iter().find(|e| e.key.as_deref() == Some(key) && e.text.as_deref() == Some(text));
    found.unwrap_or_else(|| panic!("no `{key}` showing `{text}`")).text_color.expect("the text has a colour")
}

/// The texts of the Session header's totals, which sit in the element keyed `changeset-totals`
/// inside the header.
fn totals(rt: &Runtime<State>) -> Vec<String> {
    let header = texts_within(rt, "session-header");
    let totals = texts_within(rt, "changeset-totals");
    assert!(totals.iter().all(|t| header.contains(t)), "the totals aren't in the Session header");
    totals
}

/// Opens a Session over `repo`'s uncommitted work and shows it in a window.
fn review(repo: &TestRepo) -> (Arc<Service>, Runtime<State>) {
    let service = Arc::new(Service::new());
    let _pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    let rt = window(&service);
    (service, rt)
}

#[test]
fn a_local_sessions_header_totals_its_changeset() {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n").write("b.txt", "one\ntwo\n").commit_all("base");
    repo.write("a.txt", "two\n").write("b.txt", "one\ntwo\nthree\nfour\n");

    let (_service, rt) = review(&repo);

    assert_eq!(totals(&rt), ["2 files", "+3", "−1"]);
}

#[test]
fn one_changed_file_reads_one_file() {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n").commit_all("base");
    repo.write("a.txt", "two\n");

    let (_service, rt) = review(&repo);

    assert_eq!(totals(&rt), ["1 file", "+1", "−1"]);
}

#[test]
fn an_empty_changeset_reads_zero_files_and_zero_lines() {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n").commit_all("base");

    let (_service, rt) = review(&repo);

    assert_eq!(totals(&rt), ["0 files", "+0", "−0"]);
}

#[test]
fn a_binary_file_counts_as_a_file_and_adds_no_lines() {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n").write_bytes("logo.png", &[0, 1, 2, 3]).commit_all("base");
    repo.write("a.txt", "two\n").write_bytes("logo.png", &[0, 9, 8, 7, 6]);

    let (_service, rt) = review(&repo);

    assert_eq!(totals(&rt), ["2 files", "+1", "−1"]);
}

#[test]
fn a_generated_file_and_a_viewed_file_still_count() {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n").write("b.txt", "one\n").write("go.sum", "one\n").commit_all("base");
    repo.write("a.txt", "two\n").write("b.txt", "two\n").write("go.sum", "two\n");
    let service = Arc::new(Service::new());
    let pending = service.review(ReviewRequest::new(repo.path())).unwrap();
    service.mark_viewed(pending.session(), "a.txt", true).unwrap();

    let rt = window(&service);

    assert_eq!(totals(&rt), ["3 files", "+3", "−3"]);
}

#[test]
fn a_large_file_still_counts() {
    let repo = TestRepo::new();
    repo.write("big.txt", "").commit_all("base");
    repo.write("big.txt", &"line\n".repeat(LARGE));

    let (_service, rt) = review(&repo);

    assert_eq!(totals(&rt), ["1 file", "+2001", "−0"]);
}

#[test]
fn the_totals_follow_the_changeset_view() {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n").commit_all("base");
    repo.write("a.txt", "two\n");
    let service = Arc::new(Service::new());
    let first = service.review(ReviewRequest::new(repo.path())).unwrap();
    let id = first.session().clone();
    let mut rt = window(&service);
    service.comment(&id, Anchor::lines(Side::New, "a.txt", 1..=1), "Why two?").unwrap();
    service.hand_over_round(&id).unwrap();
    assert!(first.wait_timeout(Duration::from_secs(5)).is_some(), "the Round wasn't handed over");
    repo.write("a.txt", "two\nthree\nfour\n");
    let _second = service.review(ReviewRequest::new(repo.path())).unwrap();
    service.show(&id, ChangesetView::Full).unwrap();
    deliver_window(&mut rt, &service);
    assert_eq!(totals(&rt), ["1 file", "+3", "−1"], "the whole Changeset's totals");

    service.show(&id, ChangesetView::SinceLastRound).unwrap();
    deliver_window(&mut rt, &service);

    assert_eq!(totals(&rt), ["1 file", "+2", "−0"], "the since-last-Round totals");
}

#[test]
fn the_totals_use_the_colours_of_the_per_file_counts() {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n").commit_all("base");
    repo.write("a.txt", "two\n");

    let (_service, rt) = review(&repo);

    assert_eq!(colour_of(&rt, "total-adds", "+1"), colour_of(&rt, "adds", "+1"), "added colour");
    assert_eq!(colour_of(&rt, "total-dels", "−1"), colour_of(&rt, "dels", "−1"), "removed colour");
}

#[test]
fn a_zero_count_is_still_written_in_the_colour_of_its_kind() {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n").commit_all("base");
    repo.write("a.txt", "one\ntwo\n");

    let (_service, rt) = review(&repo);

    assert_eq!(totals(&rt), ["1 file", "+1", "−0"]);
    assert_ne!(colour_of(&rt, "total-adds", "+1"), colour_of(&rt, "total-dels", "−0"), "both counts share a colour");
}

const PR_URL: &str = "https://forge.test/acme/web/pull/7";

/// A Forge with one Pull Request, `acme/web#7`, whose `login.rs` goes from one line to three.
struct FakeForge;

impl ForgeAdapter for FakeForge {
    fn host(&self) -> &str {
        "forge.test"
    }

    fn recognise(&self, url: &str) -> Option<PullRequestRef> {
        url.starts_with(PR_URL).then(|| PullRequestRef {
            host: "forge.test".into(),
            repo: "acme/web".into(),
            number: 7,
        })
    }

    fn pull_request(&self, id: &PullRequestRef) -> Result<PullRequest, ForgeError> {
        Ok(PullRequest {
            id: id.clone(),
            url: PR_URL.into(),
            title: "Add SSO login".into(),
            author: "maria".into(),
            head_branch: "sso".into(),
            head_sha: "headsha".into(),
            base_sha: "basesha".into(),
        })
    }

    fn changeset(&self, _: &PullRequest) -> Result<Changeset, ForgeError> {
        let login = FileChange::text("login.rs", Some("fn login() {}\n"), Some("fn login() {\n    sso();\n}\n"));
        Ok(Changeset { base: "basesha".into(), files: vec![login] })
    }

    fn threads(&self, _: &PullRequest) -> Result<Vec<RemoteThread>, ForgeError> {
        Ok(Vec::new())
    }

    fn viewer(&self) -> Result<String, ForgeError> {
        Ok("me".into())
    }

    fn verdicts(&self, _: &PullRequest, _: bool) -> Result<Vec<Verdict>, ForgeError> {
        Ok(Verdict::ALL.to_vec())
    }

    fn publish(&self, _: &PullRequest, _: &Review) -> Result<String, ForgeError> {
        Ok(format!("{PR_URL}#pullrequestreview-1"))
    }
}

#[test]
fn a_pull_request_sessions_header_totals_its_changeset() {
    let service = Arc::new(Service::new().with_forges(vec![Box::new(FakeForge)]));
    service.open_pull_request(PR_URL).unwrap();

    let rt = window(&service);

    assert_eq!(totals(&rt), ["1 file", "+3", "−1"]);
}
