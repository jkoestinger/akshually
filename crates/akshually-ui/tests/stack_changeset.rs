//! UI: the Stack Changeset in a Stack's Session, in Rune's headless `Runtime` against a real
//! Session in the core service: the picker's last entry, and what the Window shows and hides
//! while the Stack Changeset is selected, which is for reading only (ADR 0004).

use std::sync::Arc;

use rune::accesskit::Action;
use rune::{Env, Runtime};

use akshually_core::forge::{Comparison, ForgeAdapter, ForgeError, PullRequest, PullRequestRef, RemoteThread, Review};
use akshually_core::{Anchor, Changeset, FileChange, Service, SessionId, Verdict};
use akshually_ui::{State, view};

/// A Forge whose open Pull Requests make a Stack: `#1 <- #2 <- #3`, on branches `a`, `b` and `c`.
struct StackForge {
    /// Comparing two commits fails.
    broken: bool,
    /// The Forge cuts the comparison short.
    cut_off: bool,
}

fn pr_ref(number: u64) -> PullRequestRef {
    PullRequestRef { host: "forge.test".into(), repo: "acme/web".into(), number }
}

fn url(number: u64) -> String {
    format!("https://forge.test/acme/web/pull/{number}")
}

fn pull_request(number: u64) -> PullRequest {
    let (head, base) = [("a", "main"), ("b", "a"), ("c", "b")][number as usize - 1];
    PullRequest {
        id: pr_ref(number),
        url: url(number),
        title: ["Add the store", "Add SSO login", "Add the audit log"][number as usize - 1].into(),
        author: "maria".into(),
        head_branch: head.into(),
        head_sha: format!("head{number}"),
        base_sha: format!("base{number}"),
        base_branch: base.into(),
        from_fork: false,
        ..Default::default()
    }
}

impl ForgeAdapter for StackForge {
    fn host(&self) -> &str {
        "forge.test"
    }

    fn recognise(&self, url: &str) -> Option<PullRequestRef> {
        let number = url.strip_prefix("https://forge.test/acme/web/pull/")?.parse().ok()?;
        Some(pr_ref(number))
    }

    fn pull_request(&self, id: &PullRequestRef) -> Result<PullRequest, ForgeError> {
        Ok(pull_request(id.number))
    }

    fn open_with_head_branch(&self, _: &str, branch: &str) -> Result<Vec<PullRequest>, ForgeError> {
        Ok((1..=3).map(pull_request).filter(|p| p.head_branch == branch).collect())
    }

    fn open_with_target_branch(&self, _: &str, branch: &str) -> Result<Vec<PullRequest>, ForgeError> {
        Ok((1..=3).map(pull_request).filter(|p| p.base_branch == branch).collect())
    }

    fn changeset(&self, pr: &PullRequest) -> Result<Changeset, ForgeError> {
        let path = format!("f{}.rs", pr.id.number);
        Ok(Changeset { base: pr.base_sha.clone(), files: vec![FileChange::text(&path, Some("a\n"), Some("b\n"))] })
    }

    fn changeset_between(&self, _: &str, base: &str, _: &str) -> Result<Comparison, ForgeError> {
        if self.broken {
            let message = "compare is down".to_owned();
            return Err(ForgeError::Api { host: "forge.test".into(), status: 500, message });
        }
        let file = FileChange::text("whole.rs", Some("a\n"), Some("added by the whole Stack\n"));
        let cut_off = self.cut_off.then(|| "GitHub lists at most 300 files of a comparison".to_owned());
        Ok(Comparison { changeset: Changeset { base: base.into(), files: vec![file] }, cut_off })
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

    fn publish(&self, pr: &PullRequest, _: &Review) -> Result<String, ForgeError> {
        Ok(format!("{}#review", pr.url))
    }
}

/// `StackForge` where `#1` has nothing on top of it.
struct OneAlone;

impl ForgeAdapter for OneAlone {
    fn host(&self) -> &str {
        "forge.test"
    }
    fn recognise(&self, url: &str) -> Option<PullRequestRef> {
        StackForge { broken: false, cut_off: false }.recognise(url)
    }
    fn pull_request(&self, id: &PullRequestRef) -> Result<PullRequest, ForgeError> {
        Ok(pull_request(id.number))
    }
    fn changeset(&self, pr: &PullRequest) -> Result<Changeset, ForgeError> {
        StackForge { broken: false, cut_off: false }.changeset(pr)
    }
    fn open_with_head_branch(&self, _: &str, _: &str) -> Result<Vec<PullRequest>, ForgeError> {
        Ok(Vec::new())
    }
    fn open_with_target_branch(&self, _: &str, _: &str) -> Result<Vec<PullRequest>, ForgeError> {
        Ok(Vec::new())
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
    fn publish(&self, pr: &PullRequest, _: &Review) -> Result<String, ForgeError> {
        Ok(format!("{}#review", pr.url))
    }
}

/// The Stack's Session on its middle Member, which holds a General Thread.
fn stack_session(forge: StackForge) -> (Arc<Service>, SessionId) {
    let service = Arc::new(Service::new().with_forges(vec![Box::new(forge)]));
    let id = service.open_pull_request(&url(2)).unwrap();
    service.comment(&id, Anchor::Review, "A general remark.").unwrap();
    (service, id)
}

fn window(service: &Arc<Service>) -> Runtime<State> {
    let mut rt = Runtime::new(State::new(service.clone()), view, (1280.0, 800.0), Env::default());
    rt.frame(0.0);
    rt
}

fn press(rt: &mut Runtime<State>, key: &str) {
    assert!(rt.accessibility_action(key, Action::Click, None), "nothing to press at `{key}`");
    rt.frame(0.0);
}

/// Hands the window what the service changed, as its subscriber does, and lets what left it fade.
fn deliver(rt: &mut Runtime<State>, service: &Arc<Service>) {
    let window = service.window();
    rt.update_if(move |s: &mut State, _| {
        s.apply(window);
        true
    });
    for _ in 0..4 {
        rt.frame(1.0);
    }
}

/// Chooses the picker's `option`th entry, 0-based.
fn choose(rt: &mut Runtime<State>, service: &Arc<Service>, option: usize) {
    press(rt, "stack-member-trigger");
    press(rt, &format!("option{option}"));
    deliver(rt, service);
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

fn has_text(rt: &Runtime<State>, key: &str, text: &str) -> bool {
    texts_within(rt, key).iter().any(|t| t.contains(text))
}

#[test]
fn the_picker_ends_with_the_whole_stack_for_a_stack_of_three() {
    let (service, _) = stack_session(StackForge { broken: false, cut_off: false });
    let mut rt = window(&service);

    press(&mut rt, "stack-member-trigger");

    let listed = texts_within(&rt, "stack-member-content");
    assert_eq!(listed.last().map(String::as_str), Some("Whole Stack"), "{listed:?}");
    assert_eq!(listed.len(), 4, "the three members, then it: {listed:?}");
}

#[test]
fn a_stack_of_one_has_no_picker_to_offer_the_whole_stack_in() {
    let service = Arc::new(Service::new().with_forges(vec![Box::new(OneAlone)]));
    service.open_pull_request(&url(1)).unwrap();

    let rt = window(&service);

    assert!(rt.find("stack-member-trigger").is_none());
}

#[test]
fn choosing_the_whole_stack_shows_its_diff_for_reading_only() {
    let (service, id) = stack_session(StackForge { broken: false, cut_off: false });
    let mut rt = window(&service);
    assert!(has_text(&rt, "session-header", "Publish…"), "the member's header offers Publish");
    assert!(rt.find("general-threads").is_some(), "the member's General Thread shows");
    assert!(rt.find("plus").is_some(), "the member's lines take a Comment");
    assert!(has_text(&rt, &format!("diff-{id}"), "Viewed"), "the member's files can be ticked");

    choose(&mut rt, &service, 3);

    let drawn = texts_within(&rt, &format!("diff-{id}"));
    assert!(drawn.iter().any(|t| t.contains("whole.rs")), "the diff of the Stack Changeset: {drawn:?}");
    assert!(has_text(&rt, "session-header", "Whole Stack"));
    assert!(
        !has_text(&rt, "session-header", "Publish…"),
        "Publish is hidden: {:?}",
        texts_within(&rt, "session-header")
    );
    assert!(!has_text(&rt, "session-header", "Comment on Review"), "no Comment on the Review");
    assert!(!has_text(&rt, "session-header", "Drafts"), "no Drafts of a member");
    assert!(rt.find("general-threads").is_none(), "General Threads are hidden");
    assert!(rt.find("changeset-view-trigger").is_none(), "no Changeset view picker");
    assert!(rt.find("plus").is_none(), "no way to start a Thread on a line");
    assert!(!has_text(&rt, &format!("diff-{id}"), "Comment on file"), "none on a file either");
    assert!(!has_text(&rt, &format!("diff-{id}"), "Viewed"), "no file of the Stack Changeset is a Member's to tick");
}

#[test]
fn a_stack_changeset_the_forge_cut_short_says_it_is_not_complete() {
    let (service, _) = stack_session(StackForge { broken: false, cut_off: true });
    let mut rt = window(&service);
    assert!(rt.find("stack-changeset-cut-off").is_none(), "nothing is cut off on a member");

    choose(&mut rt, &service, 3);

    let notice = texts_within(&rt, "stack-changeset-cut-off");
    assert!(notice.iter().any(|t| t.contains("cut off") && t.contains("300 files")), "{notice:?}");
}

#[test]
fn choosing_a_member_again_brings_back_its_header_and_its_threads() {
    let (service, _) = stack_session(StackForge { broken: false, cut_off: false });
    let mut rt = window(&service);
    choose(&mut rt, &service, 3);

    choose(&mut rt, &service, 1);

    assert!(has_text(&rt, "session-header", "Publish…"));
    assert!(rt.find("general-threads").is_some());
    assert!(rt.find("plus").is_some());
}

#[test]
fn a_failed_fetch_shows_the_error_in_place_of_the_diff_and_the_members_stay_choosable() {
    let (service, id) = stack_session(StackForge { broken: true, cut_off: false });
    let mut rt = window(&service);

    choose(&mut rt, &service, 3);

    let error = texts_within(&rt, "stack-changeset-error");
    assert!(error.iter().any(|t| t.contains("compare is down")), "{error:?}");
    assert!(rt.find(&format!("diff-{id}")).is_none(), "the diff of a Changeset that wasn't fetched");
    choose(&mut rt, &service, 0);
    assert!(has_text(&rt, "session-header", "Publish…"));
    assert!(rt.find("stack-changeset-error").is_none());
}
