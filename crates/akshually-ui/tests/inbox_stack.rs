//! UI: a Stack's Pull Requests grouped in the rendered Inbox, in Rune's headless `Runtime` against
//! the core service with a fake Forge (ADR 0004).

use std::sync::Arc;

use rune::{Env, Runtime};

use akshually_core::forge::{
    ForgeAdapter, ForgeError, ForgeKind, InboxItem, InboxRole, PullRequest, PullRequestRef, RemoteThread, Review,
};
use akshually_core::{Changeset, Service, Verdict};
use akshually_ui::{State, view};

/// A Forge whose open Pull Requests make a Stack `#1 <- #2 <- #3`, and a lone `#4`. The human's
/// review is requested on `#3` and `#4`.
struct Forge;

fn pr_ref(number: u64) -> PullRequestRef {
    PullRequestRef { host: "forge.test".into(), repo: "acme/web".into(), number }
}

fn pull_request(number: u64) -> PullRequest {
    let (head, base) = [("a", "main"), ("b", "a"), ("c", "b"), ("d", "main")][number as usize - 1];
    PullRequest {
        id: pr_ref(number),
        url: format!("https://forge.test/acme/web/pull/{number}"),
        title: format!("Pull Request {number}"),
        author: "maria".into(),
        head_branch: head.into(),
        head_sha: format!("head{number}"),
        base_sha: format!("base{number}"),
        base_branch: base.into(),
        from_fork: false,
    }
}

impl ForgeAdapter for Forge {
    fn host(&self) -> &str {
        "forge.test"
    }

    fn recognise(&self, url: &str) -> Option<PullRequestRef> {
        Some(pr_ref(url.strip_prefix("https://forge.test/acme/web/pull/")?.parse().ok()?))
    }

    fn pull_request(&self, id: &PullRequestRef) -> Result<PullRequest, ForgeError> {
        Ok(pull_request(id.number))
    }

    fn open_with_head_branch(&self, _: &str, branch: &str) -> Result<Vec<PullRequest>, ForgeError> {
        Ok((1..=4).map(pull_request).filter(|p| p.head_branch == branch).collect())
    }

    fn open_with_target_branch(&self, _: &str, branch: &str) -> Result<Vec<PullRequest>, ForgeError> {
        Ok((1..=4).map(pull_request).filter(|p| p.base_branch == branch).collect())
    }

    fn changeset(&self, pr: &PullRequest) -> Result<Changeset, ForgeError> {
        Ok(Changeset { base: pr.base_sha.clone(), files: Vec::new() })
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

    fn inbox(&self) -> Result<Option<Vec<InboxItem>>, ForgeError> {
        let item = |number: u64| {
            let pr = pull_request(number);
            InboxItem {
                id: pr.id,
                kind: ForgeKind::GitHub,
                url: pr.url,
                title: pr.title,
                author: pr.author,
                created_at: 1_000,
                updated_at: number as i64,
                role: InboxRole::Reviewer,
                approved: false,
                verdict_given: false,
            }
        };
        Ok(Some(vec![item(3), item(4)]))
    }
}

fn inbox_window() -> Runtime<State> {
    let service = Arc::new(Service::new().with_forges(vec![Box::new(Forge)]));
    service.refresh_inbox();
    let mut rt = Runtime::new(State::new(service), view, (1280.0, 800.0), Env::default());
    rt.frame(0.0);
    rt
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
fn a_stack_is_set_apart_as_a_group_with_its_members_in_order_and_the_unrequested_ones_marked() {
    let rt = inbox_window();

    let group = texts_within(&rt, "inbox-stack-https://forge.test/acme/web/pull/1");

    let titles: Vec<_> = group.iter().filter(|t| t.starts_with("Pull Request ")).collect();
    assert_eq!(titles, ["Pull Request 1", "Pull Request 2", "Pull Request 3"], "{group:?}");
    let marks = group.iter().filter(|t| *t == "Not requested").count();
    assert_eq!(marks, 2, "{group:?}");
    assert!(group.contains(&"Requested reviewer".to_owned()), "{group:?}");
}

#[test]
fn a_lone_pull_request_is_a_plain_row_outside_any_group() {
    let rt = inbox_window();

    let lone = texts_within(&rt, "inbox-https://forge.test/acme/web/pull/4");

    assert!(lone.contains(&"Pull Request 4".to_owned()), "{lone:?}");
    assert!(lone.contains(&"Requested reviewer".to_owned()), "{lone:?}");
    assert!(
        !texts_within(&rt, "inbox-stack-https://forge.test/acme/web/pull/1").contains(&"Pull Request 4".to_owned()),
        "inside the group"
    );
    assert!(
        !texts_within(&rt, "inbox-stack-https://forge.test/acme/web/pull/4").iter().any(|t| t == "Pull Request 4"),
        "a group of one"
    );
}
