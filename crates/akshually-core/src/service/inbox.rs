//! The Inbox: the open Pull Requests, across every Forge host, where the human is a requested
//! reviewer or an assignee. Refreshed on a timer and on demand; a new review request marks it.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use super::{Inner, Service, Session, SessionId};
use crate::forge::{InboxItem, InboxRole, PullRequestRef};

/// What the UI (and tests) can observe of the Inbox.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InboxView {
    /// Most recently updated first.
    pub rows: Vec<InboxRow>,
    /// When the last refresh finished, in seconds since the Unix epoch; `None` before the first.
    pub refreshed_at: Option<i64>,
    /// Why hosts failed in the last refresh; their rows are the ones from before.
    pub errors: Vec<String>,
    /// A new review request arrived while the Inbox wasn't displayed, and it hasn't been since.
    pub attention: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxRow {
    pub item: InboxItem,
    pub state: InboxRowState,
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
    /// It has an open Session, holding `drafts` Drafts.
    SessionOpen { session: SessionId, drafts: usize },
}

/// The Inbox as the service keeps it, and as the store saves it.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Inbox {
    items: Vec<InboxItem>,
    /// When the human last opened each listed Pull Request: its `updated_at` then.
    seen: Vec<Seen>,
    refreshed_at: Option<i64>,
    attention: bool,
    #[serde(skip)]
    errors: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct Seen {
    pr: PullRequestRef,
    updated_at: i64,
}

type InboxSubscriber = Box<dyn Fn(&InboxView) + Send>;

impl Inbox {
    fn seen(&self, pr: &PullRequestRef) -> Option<i64> {
        self.seen.iter().find(|s| &s.pr == pr).map(|s| s.updated_at)
    }

    /// The human looked at `pr`: as of its last refresh, or now if the Inbox doesn't list it.
    pub(super) fn looked_at(&mut self, pr: &PullRequestRef) {
        let updated_at = self.items.iter().find(|i| &i.id == pr).map_or_else(now, |i| i.updated_at);
        self.seen.retain(|s| &s.pr != pr);
        self.seen.push(Seen { pr: pr.clone(), updated_at });
    }

    fn view(&self, sessions: &[Session]) -> InboxView {
        let rows = self
            .items
            .iter()
            .map(|item| {
                let session = sessions
                    .iter()
                    .find(|s| s.view.ended.is_none() && s.view.pull_request.as_ref().is_some_and(|p| p.id == item.id));
                let state = match (session, self.seen(&item.id)) {
                    (Some(s), _) => InboxRowState::SessionOpen {
                        session: s.view.id.clone(),
                        drafts: s.view.threads.iter().flat_map(|t| &t.comments).filter(|c| c.is_draft()).count(),
                    },
                    (None, None) => InboxRowState::New,
                    (None, Some(seen)) if seen < item.updated_at => InboxRowState::Updated,
                    (None, Some(_)) => InboxRowState::Seen,
                };
                InboxRow { item: item.clone(), state }
            })
            .collect();
        InboxView { rows, refreshed_at: self.refreshed_at, errors: self.errors.clone(), attention: self.attention }
    }
}

/// The Inbox-related state of the service, beside its Sessions.
#[derive(Default)]
pub(super) struct InboxState {
    pub(super) saved: Inbox,
    /// The human has the Inbox tab open (and so no Session's).
    pub(super) displayed: bool,
    subscribers: Vec<InboxSubscriber>,
}

impl Inner {
    /// Saves the Inbox and tells its subscribers.
    pub(super) fn notify_inbox(&mut self) {
        if let Some(store) = &mut self.store
            && let Err(error) = store.sync_inbox(&self.inbox.saved)
        {
            eprintln!("akshually: couldn't save the Inbox: {error}");
        }
        if self.inbox.subscribers.is_empty() {
            return;
        }
        let view = self.inbox.saved.view(&self.sessions);
        for subscriber in &self.inbox.subscribers {
            subscriber(&view);
        }
    }
}

impl Service {
    /// The Inbox as of its last refresh, with each row's state now.
    pub fn inbox(&self) -> InboxView {
        let inner = self.lock();
        inner.inbox.saved.view(&inner.sessions)
    }

    /// Calls `f` with the Inbox after each refresh, and whenever its attention changes. Like
    /// `subscribe`, `f` runs while the service is locked: hand the Inbox off, don't call back.
    pub fn subscribe_inbox(&self, f: impl Fn(&InboxView) + Send + 'static) {
        self.lock().inbox.subscribers.push(Box::new(f));
    }

    /// Asks every Forge host for its Inbox; blocks on them. A host that fails keeps its rows
    /// from before, and its error shows until a refresh succeeds. A review request that wasn't
    /// listed before marks the Inbox, unless the human has it displayed.
    pub fn refresh_inbox(&self) {
        // Not holding the lock: the Forges may take a while.
        let fetched: Vec<_> = self.forges.iter().map(|forge| (forge, forge.inbox())).collect();
        let mut items = Vec::new();
        let mut errors = Vec::new();
        let mut inner = self.lock();
        let before = std::mem::take(&mut inner.inbox.saved.items);
        for (forge, result) in fetched {
            match result {
                Ok(found) => items.extend(found),
                Err(error) => {
                    errors.push(error.to_string());
                    items.extend(before.iter().filter(|i| forge.recognise(&i.url).is_some()).cloned());
                }
            }
        }
        let mut listed = Vec::new();
        items.retain(|i: &InboxItem| {
            let first = !listed.contains(&i.id);
            listed.push(i.id.clone());
            first
        });
        items.sort_by_key(|i| std::cmp::Reverse(i.updated_at));

        let requested = |items: &[InboxItem], pr: &PullRequestRef| {
            items.iter().any(|i| &i.id == pr && i.role == InboxRole::Reviewer)
        };
        let new_request = items.iter().any(|i| i.role == InboxRole::Reviewer && !requested(&before, &i.id));
        let state = &mut inner.inbox;
        if new_request && !state.displayed {
            state.saved.attention = true;
        }
        // What the human saw of a Pull Request that left the Inbox no longer matters.
        state.saved.seen.retain(|s| items.iter().any(|i| i.id == s.pr));
        state.saved.items = items;
        state.saved.errors = errors;
        state.saved.refreshed_at = Some(now());
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

    /// Human: opens the Inbox tab, which clears its attention. Sessions that need the human
    /// meanwhile get attention rather than the view.
    pub fn display_inbox(&self) {
        let mut inner = self.lock();
        inner.displayed = None;
        inner.inbox.displayed = true;
        if inner.inbox.saved.attention {
            inner.inbox.saved.attention = false;
            inner.notify_inbox();
        }
    }

    /// Whether the human has the Inbox tab open.
    pub fn inbox_displayed(&self) -> bool {
        self.lock().inbox.displayed
    }
}

/// Seconds since the Unix epoch.
fn now() -> i64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}
