//! The Inbox: the open Pull Requests, across every Forge host, where the human is a requested
//! reviewer or an assignee, leaving out those already approved and those the human gave a Verdict
//! on since their review was last requested, unless they have a Session open on them. Refreshed
//! on a timer and on demand, deciding afresh each time; a new review request marks it. A Stack
//! with a Pull Request in it is shown whole, as one group, including the Members that don't
//! qualify on their own.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use super::stack::{detect_stack, session_run};
use super::{Inner, Service, Session, SessionId};
use crate::forge::{ForgeAdapter, ForgeError, InboxItem, InboxRole, PullRequest, PullRequestRef};

/// What the UI (and tests) can observe of the Inbox.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InboxView {
    /// Most recently updated first. The Members of a Stack follow each other, bottom first, at
    /// the place of the group's most recently updated listed Member.
    pub rows: Vec<InboxRow>,
    /// When the last refresh finished, in seconds since the Unix epoch; `None` before the first.
    pub refreshed_at: Option<i64>,
    /// Why hosts failed in the last refresh; their rows are the ones from before.
    pub errors: Vec<String>,
    /// The hosts the last refresh asked, whether they answered or failed, in the adapters'
    /// order. A host that isn't set up isn't listed.
    pub hosts: Vec<String>,
    /// A new review request arrived while the Inbox wasn't displayed, and it hasn't been since.
    pub attention: bool,
}

/// One Pull Request in the Inbox, and what its row tells the human about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxRow {
    pub item: InboxItem,
    pub state: InboxRowState,
    /// Whether the Pull Request is in the Inbox on its own, and if not, why a Member of a Stack
    /// is shown anyway.
    pub standing: InboxStanding,
    /// Where the row sits in its Stack's group; `None` for a Stack of one, or when the Stack
    /// couldn't be looked up.
    pub stack: Option<InboxStack>,
}

/// Whether a row is in the Inbox in its own right: the human is a requested reviewer or an
/// assignee, it isn't approved yet, and they gave no Verdict since their review was last
/// requested. A Member of a Stack that isn't is shown only because its Stack is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InboxStanding {
    /// It qualifies on its own.
    Listed,
    /// The human is neither a requested reviewer nor an assignee of it.
    NotRequested,
    /// The human gave a Verdict on it since their review was last requested. It wins over
    /// `Approved` when both apply: what the human did is what explains the row.
    VerdictGiven,
    /// It meets its approval requirement already.
    Approved,
}

impl InboxRow {
    /// The Pull Request is in the Inbox on its own.
    pub fn qualifies(&self) -> bool {
        self.standing == InboxStanding::Listed
    }
}

/// A row's place in the group of its Stack's Pull Requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InboxStack {
    /// 0 for the bottom Member.
    pub position: usize,
    /// How many Members the group shows.
    pub size: usize,
}

/// What a row tells the human, besides the Pull Request itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InboxRowState {
    /// The human hasn't opened it.
    New,
    /// The Pull Request changed since the human last opened it.
    Updated,
    /// Nothing happened since the human last opened it.
    Seen,
    /// It has an open Session, holding `drafts` Drafts in this Pull Request's Review.
    SessionOpen { session: SessionId, drafts: usize },
}

/// The Inbox as the service keeps it, and as the store saves it.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Inbox {
    items: Vec<InboxItem>,
    /// The Stacks of more than one Pull Request with a listed Member, each bottom to top.
    groups: Vec<Vec<PullRequestRef>>,
    /// The Members of `groups` that aren't in `items`: shown because their Stack is.
    others: Vec<InboxItem>,
    /// When the human last opened each listed Pull Request: its `updated_at` then.
    seen: Vec<Seen>,
    refreshed_at: Option<i64>,
    attention: bool,
    #[serde(skip)]
    errors: Vec<String>,
    /// Saved with `refreshed_at`, which it goes with, so the Inbox still names them on launch.
    hosts: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct Seen {
    pr: PullRequestRef,
    updated_at: i64,
}

impl Inbox {
    fn seen(&self, pr: &PullRequestRef) -> Option<i64> {
        self.seen.iter().find(|s| &s.pr == pr).map(|s| s.updated_at)
    }

    /// `pr` as a row of the Inbox: listed in its own right, or as a Member of a group.
    fn listed(&self, pr: &PullRequestRef) -> Option<&InboxItem> {
        self.items.iter().chain(&self.others).find(|i| &i.id == pr)
    }

    /// The human looked at `pr`: as of its last refresh, or now if the Inbox doesn't list it.
    pub(super) fn looked_at(&mut self, pr: &PullRequestRef) {
        let updated_at = self.listed(pr).map_or_else(now, |i| i.updated_at);
        self.seen.retain(|s| &s.pr != pr);
        self.seen.push(Seen { pr: pr.clone(), updated_at });
    }

    /// The human looked at the Inbox: clears its attention. Whether it had any.
    pub(super) fn clear_attention(&mut self) -> bool {
        std::mem::take(&mut self.attention)
    }

    /// The human Published a review of `pr`: the update that makes on the Forge is theirs, so
    /// it doesn't make the row Updated.
    pub(super) fn published(&mut self, pr: &PullRequestRef) {
        self.seen.retain(|s| &s.pr != pr);
        self.seen.push(Seen { pr: pr.clone(), updated_at: now() });
    }

    pub(super) fn view(&self, sessions: &[Session]) -> InboxView {
        let row = |item: &InboxItem, stack: Option<InboxStack>| {
            // A Published Session stays open until the human closes it.
            let session = sessions.iter().find(|s| s.view.has_member(&item.id));
            let state = match (session, self.seen(&item.id)) {
                (Some(s), _) => InboxRowState::SessionOpen {
                    session: s.view.id.clone(),
                    drafts: s.view.stack().into_iter().find(|m| m.pull_request.id == item.id).map_or(0, |m| m.drafts),
                },
                (None, None) => InboxRowState::New,
                (None, Some(seen)) if seen < item.updated_at => InboxRowState::Updated,
                (None, Some(_)) => InboxRowState::Seen,
            };
            let standing = match (item.verdict_given, item.approved) {
                (true, _) => InboxStanding::VerdictGiven,
                (_, true) => InboxStanding::Approved,
                _ if self.items.iter().any(|i| i.id == item.id) => InboxStanding::Listed,
                _ => InboxStanding::NotRequested,
            };
            InboxRow { item: item.clone(), state, standing, stack }
        };
        let mut rows = Vec::new();
        for item in &self.items {
            let Some(group) = self.groups.iter().find(|g| g.contains(&item.id)) else {
                rows.push(row(item, None));
                continue;
            };
            // The group takes the place of its most recently updated Member that qualifies, or of
            // one a Session keeps listed when none does.
            let in_group = |qualifying: bool| {
                let qualifies = |i: &InboxItem| !qualifying || !i.done();
                self.items.iter().find(|i| group.contains(&i.id) && qualifies(i))
            };
            if in_group(true).or_else(|| in_group(false)).map(|i| &i.id) != Some(&item.id) {
                continue;
            }
            let members: Vec<_> = group.iter().filter_map(|id| self.listed(id)).collect();
            for (position, member) in members.iter().enumerate() {
                rows.push(row(member, Some(InboxStack { position, size: members.len() })));
            }
        }
        InboxView {
            rows,
            refreshed_at: self.refreshed_at,
            errors: self.errors.clone(),
            hosts: self.hosts.clone(),
            attention: self.attention,
        }
    }
}

impl Inner {
    /// Saves the Inbox and tells the Window subscribers.
    pub(super) fn notify_inbox(&mut self) {
        if let Some(store) = &mut self.store
            && let Err(error) = store.sync_inbox(&self.inbox)
        {
            eprintln!("akshually: couldn't save the Inbox: {error}");
        }
        self.tell_window_subscribers();
    }
}

impl Service {
    /// The Inbox as of its last refresh, with each row's state now.
    pub fn inbox(&self) -> InboxView {
        let inner = self.lock();
        inner.inbox.view(&inner.sessions)
    }

    /// Asks every Forge host for its Inbox, then for the Stack of each Pull Request it lists
    /// (the way a Session does); blocks on them. A host that fails keeps its rows and groups
    /// from before, and its error shows until a refresh succeeds; one that isn't set up is
    /// skipped. A host whose Stack lookup fails shows its rows ungrouped, and that error shows
    /// too. A review request that wasn't listed before marks the Inbox, unless the human
    /// opened it and the Window displays it.
    pub fn refresh_inbox(&self) {
        // Not holding the lock: the Forges may take a while.
        let fetched: Vec<_> = self.forges.iter().map(|forge| (forge, forge.inbox())).collect();
        let mut items = Vec::new();
        let mut errors = Vec::new();
        let mut hosts = Vec::new();
        // The hosts that answered, whose Stacks can be looked up.
        let mut answering: Vec<&dyn ForgeAdapter> = Vec::new();
        // What a host that failed keeps from before besides its rows: its groups.
        let mut carried = Grouped::default();
        let inner = self.lock();
        let before = inner.inbox.items.clone();
        for (forge, result) in fetched {
            match result {
                Ok(None) => {}
                Ok(Some(found)) => {
                    hosts.push(forge.host().to_owned());
                    answering.push(forge.as_ref());
                    items.extend(found);
                }
                Err(error) => {
                    hosts.push(forge.host().to_owned());
                    errors.push(error.to_string());
                    items.extend(before.iter().filter(|i| forge.recognise(&i.url).is_some()).cloned());
                    let of_host = |pr: &PullRequestRef| pr.host == forge.host();
                    carried.groups.extend(inner.inbox.groups.iter().filter(|g| g.iter().all(of_host)).cloned());
                    carried.others.extend(inner.inbox.others.iter().filter(|i| of_host(&i.id)).cloned());
                }
            }
        }
        // A Pull Request listed in both roles is a review request: that copy goes first, so it's
        // the one kept.
        items.sort_by_key(|i: &InboxItem| i.role != Some(InboxRole::Reviewer));
        let mut listed = Vec::new();
        items.retain(|i: &InboxItem| {
            let first = !listed.contains(&i.id);
            listed.push(i.id.clone());
            first
        });
        // Done with, unless the human is still reviewing it: their Session keeps it listed.
        let held = |inner: &Inner, pr: &PullRequestRef| inner.sessions.iter().any(|s| s.view.has_member(pr));
        let (kept, done): (Vec<_>, Vec<_>) = items.into_iter().partition(|i| !i.done() || held(&inner, &i.id));
        let mut items = kept;
        items.sort_by_key(|i| std::cmp::Reverse(i.updated_at));
        let sessions = inner.holdings();
        drop(inner);

        // Not holding the lock: the Forges may take a while.
        let mut grouped = group_stacks(&answering, &items, &done, &sessions);
        errors.extend(std::mem::take(&mut grouped.errors));
        grouped.groups.extend(carried.groups);
        grouped.others.extend(carried.others);
        let mut inner = self.lock();

        let requested = |items: &[InboxItem], pr: &PullRequestRef| {
            items.iter().any(|i| &i.id == pr && i.role == Some(InboxRole::Reviewer))
        };
        let new_request = items.iter().any(|i| i.role == Some(InboxRole::Reviewer) && !requested(&before, &i.id));
        let opened = inner.window.inbox_opened();
        let state = &mut inner.inbox;
        if new_request && !opened {
            state.attention = true;
        }
        // What the human saw of a Pull Request that left the Inbox no longer matters.
        state.seen.retain(|s| items.iter().chain(&grouped.others).any(|i| i.id == s.pr));
        state.items = items;
        state.groups = grouped.groups;
        state.others = grouped.others;
        state.errors = errors;
        state.hosts = hosts;
        state.refreshed_at = Some(now());
        inner.notify_inbox();
    }

    /// Runs `refresh_inbox` now, then every `every`, on a background thread, until the service
    /// is dropped.
    pub fn watch_inbox(self: &Arc<Self>, every: Duration) {
        let service = Arc::downgrade(self);
        std::thread::spawn(move || {
            loop {
                let Some(svc) = service.upgrade() else { return };
                svc.refresh_inbox();
                drop(svc);
                std::thread::sleep(every);
            }
        });
    }
}

/// The Stacks of the Inbox's Pull Requests.
#[derive(Default)]
struct Grouped {
    groups: Vec<Vec<PullRequestRef>>,
    others: Vec<InboxItem>,
    errors: Vec<String>,
}

/// Groups the Stacks of `items`, each looked up the way a Session does it, so that the Inbox and
/// the Session it opens agree on the Members. `done` are the Forge's listed Pull Requests that
/// the Inbox leaves out: a Member of a Stack that is one of them keeps its own data. `sessions`
/// are the open Sessions and their Members: the Stack stops at a Pull Request another Session
/// holds. Blocks on the network.
///
/// A host whose lookup fails leaves its rows ungrouped, and reports the failure once.
fn group_stacks(
    answering: &[&dyn ForgeAdapter],
    items: &[InboxItem],
    done: &[InboxItem],
    sessions: &[(SessionId, Vec<PullRequestRef>)],
) -> Grouped {
    let mut grouped = Grouped::default();
    let mut covered: Vec<PullRequestRef> = Vec::new();
    let mut failed: Vec<&str> = Vec::new();
    for item in items {
        if covered.contains(&item.id) {
            continue;
        }
        let Some(forge) = answering.iter().find(|f| f.recognise(&item.url).is_some()) else { continue };
        if failed.contains(&forge.host()) {
            continue;
        }
        let stack = match stack_of(*forge, item, sessions) {
            Ok(stack) => stack,
            Err(error) => {
                grouped.errors.push(error.to_string());
                failed.push(forge.host());
                continue;
            }
        };
        covered.extend(stack.iter().map(|p| p.id.clone()));
        if stack.len() < 2 {
            continue;
        }
        for member in &stack {
            if items.iter().any(|i| i.id == member.id) {
                continue;
            }
            let known = done.iter().find(|i| i.id == member.id);
            grouped.others.push(known.cloned().unwrap_or_else(|| listing_of(member)));
        }
        grouped.groups.push(stack.into_iter().map(|p| p.id).collect());
    }
    // A host that failed part-way shows all its rows ungrouped.
    grouped.groups.retain(|g| g.iter().all(|pr| !failed.contains(&pr.host.as_str())));
    grouped.others.retain(|i| !failed.contains(&i.id.host.as_str()));
    grouped
}

/// The Stack of `item`, as a Session opening it would hold it. The Forge's listing already says
/// what Stack detection starts from, so the Pull Request isn't asked for again.
fn stack_of(
    forge: &dyn ForgeAdapter,
    item: &InboxItem,
    sessions: &[(SessionId, Vec<PullRequestRef>)],
) -> Result<Vec<PullRequest>, ForgeError> {
    let stack = detect_stack(forge, &item.pull_request())?;
    let owner = |p: &PullRequestRef| sessions.iter().find(|(_, members)| members.contains(p)).map(|(id, _)| id);
    Ok(session_run(stack, &item.id, owner))
}

/// A row for a Member of a Stack that the Forge didn't list for the human: it has no role, and
/// qualifies for nothing.
fn listing_of(pr: &PullRequest) -> InboxItem {
    InboxItem {
        id: pr.id.clone(),
        kind: pr.kind(),
        url: pr.url.clone(),
        title: pr.title.clone(),
        author: pr.author.clone(),
        created_at: pr.created_at,
        updated_at: pr.updated_at,
        role: None,
        base_branch: pr.base_branch.clone(),
        head_branch: pr.head_branch.clone(),
        from_fork: pr.from_fork,
        approved: false,
        verdict_given: false,
    }
}

/// Seconds since the Unix epoch.
fn now() -> i64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}
