//! UI: a Stack in the Pull Request Session's header, in Rune's headless `Runtime` against a real
//! Session in the core service: the tab's title, and the picker that lists the members bottom to
//! top and switches between them (ADR 0004).

use std::sync::Arc;

use rune::accesskit::Action;
use rune::{Env, Runtime};

use akshually_core::forge::{ForgeAdapter, ForgeError, PullRequest, PullRequestRef, RemoteThread, Review};
use akshually_core::{Anchor, Changeset, FileChange, Service, SessionId, Verdict};
use akshually_ui::{State, view};

/// A Forge whose open Pull Requests make a Stack: `#1 <- #2 <- #3`, on branches `a`, `b` and `c`.
struct StackForge;

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

/// The Stack's Session, opened on its middle member, with the Drafts and Verdict the picker shows.
fn stack_session() -> (Arc<Service>, SessionId) {
    let service = Arc::new(Service::new().with_forges(vec![Box::new(StackForge)]));
    let id = service.open_pull_request(&url(2)).unwrap();
    service.select_member(&id, &pr_ref(1)).unwrap();
    service.comment(&id, Anchor::Review, "Fine.").unwrap();
    service.publish(&id, Verdict::Approve, "").unwrap();
    service.select_member(&id, &pr_ref(3)).unwrap();
    service.comment(&id, Anchor::Review, "One.").unwrap();
    service.comment(&id, Anchor::Review, "Two.").unwrap();
    service.select_member(&id, &pr_ref(2)).unwrap();
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

#[test]
fn the_tab_is_titled_after_the_pull_request_opened_with_the_size_of_its_stack() {
    let (service, id) = stack_session();

    let rt = window(&service);

    let title = texts_within(&rt, &format!("t-{id}"));
    assert!(title.contains(&"acme/web #2 · Stack of 3".to_owned()), "{title:?}");
}

#[test]
fn a_stack_of_ones_tab_is_titled_by_its_reference_alone() {
    let service = Arc::new(Service::new().with_forges(vec![Box::new(OneAlone)]));
    let id = service.open_pull_request(&url(1)).unwrap();
    let rt = window(&service);

    let title = texts_within(&rt, &format!("t-{id}"));

    assert!(title.contains(&"acme/web #1".to_owned()), "{title:?}");
}

/// `StackForge` where `#1` has nothing on top of it.
struct OneAlone;

impl ForgeAdapter for OneAlone {
    fn host(&self) -> &str {
        StackForge.host()
    }
    fn recognise(&self, url: &str) -> Option<PullRequestRef> {
        StackForge.recognise(url)
    }
    fn pull_request(&self, id: &PullRequestRef) -> Result<PullRequest, ForgeError> {
        StackForge.pull_request(id)
    }
    fn open_with_head_branch(&self, _: &str, _: &str) -> Result<Vec<PullRequest>, ForgeError> {
        Ok(Vec::new())
    }
    fn open_with_target_branch(&self, _: &str, _: &str) -> Result<Vec<PullRequest>, ForgeError> {
        Ok(Vec::new())
    }
    fn changeset(&self, pr: &PullRequest) -> Result<Changeset, ForgeError> {
        StackForge.changeset(pr)
    }
    fn threads(&self, pr: &PullRequest) -> Result<Vec<RemoteThread>, ForgeError> {
        StackForge.threads(pr)
    }
    fn viewer(&self) -> Result<String, ForgeError> {
        StackForge.viewer()
    }
    fn verdicts(&self, pr: &PullRequest, own: bool) -> Result<Vec<Verdict>, ForgeError> {
        StackForge.verdicts(pr, own)
    }
    fn publish(&self, pr: &PullRequest, review: &Review) -> Result<String, ForgeError> {
        StackForge.publish(pr, review)
    }
}

#[test]
fn a_stack_of_one_has_no_picker() {
    let service = Arc::new(Service::new().with_forges(vec![Box::new(OneAlone)]));
    service.open_pull_request(&url(1)).unwrap();

    let rt = window(&service);

    assert!(rt.find("stack-member-trigger").is_none(), "a picker for a Stack of one");
}

#[test]
fn the_picker_lists_the_members_bottom_to_top_with_reference_title_drafts_and_verdict() {
    let (service, _) = stack_session();
    let mut rt = window(&service);

    press(&mut rt, "stack-member-trigger");

    let listed = texts_within(&rt, "stack-member-content");
    let expected = [
        "acme/web #1 Add the store · 0 Drafts · Published, Approve",
        "acme/web #2 Add SSO login · 0 Drafts",
        "acme/web #3 Add the audit log · 2 Drafts",
    ];
    for label in expected {
        assert!(listed.contains(&label.to_owned()), "no `{label}` in {listed:?}");
    }
    let position = |label: &str| listed.iter().position(|l| l == label).unwrap();
    assert!(position(expected[0]) < position(expected[1]) && position(expected[1]) < position(expected[2]));
}

#[test]
fn choosing_a_member_in_the_picker_shows_its_review() {
    let (service, id) = stack_session();
    let mut rt = window(&service);
    press(&mut rt, "stack-member-trigger");

    press(&mut rt, "option2");

    let session = service.session(&id).unwrap();
    assert_eq!(session.pull_request().unwrap().id.number, 3);
    assert_eq!(session.drafts(), 2);
}

#[test]
fn a_published_members_header_says_so() {
    let (service, id) = stack_session();
    service.select_member(&id, &pr_ref(1)).unwrap();

    let rt = window(&service);

    let header = texts_within(&rt, "session-header");
    assert!(header.contains(&"Published · Approve".to_owned()), "{header:?}");
    assert!(rt.find("stack-member-trigger").is_some());
}
