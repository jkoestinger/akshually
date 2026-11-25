//! The Stack Changeset: everything a Stack changes as one Changeset, from the bottom member's
//! base to the top member's head, for reading only (ADR 0004). It takes no Threads: a Forge
//! anchors a comment in one Pull Request's own diff.

use super::{Service, SessionId, SessionView};
use crate::Changeset;
use crate::Error;
use crate::forge::{PullRequest, PullRequestRef};

/// What a Stack Changeset was fetched for: each member still in the Stack, with its base and
/// head commit. It is fetched again when this changes.
type Members = Vec<(PullRequestRef, String, String)>;

/// A Session's Stack Changeset, and whether it is the one shown. Not saved: it is fetched again
/// when first selected after a restart.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StackChangeset {
    /// The Window shows it in place of the selected member's Changeset (see
    /// [`SessionView::stack_changeset_selected`]).
    selected: bool,
    /// The Stack Changeset fetched; empty until then, and after a failure.
    changeset: Changeset,
    /// Why fetching it failed, shown in its place.
    error: Option<String>,
    /// What it was fetched for; `None` before the first fetch.
    fetched_for: Option<Members>,
}

impl StackChangeset {
    /// The Changeset to show while it is selected.
    pub(super) fn shown(&self) -> &Changeset {
        &self.changeset
    }
}

/// The Stack as a Stack Changeset is fetched for it.
struct Fetch {
    members: Members,
    /// The bottom member, whose base the Changeset starts at and whose Forge serves it.
    bottom: PullRequest,
    /// The head commit of the top member, where it ends.
    head: String,
}

impl SessionView {
    /// Whether the Session offers the Stack Changeset: its Stack has two or more members. A
    /// member no longer part of the Stack is not counted.
    pub fn offers_stack_changeset(&self) -> bool {
        self.stack().iter().filter(|m| m.in_stack).count() >= 2
    }

    /// Whether the Stack Changeset is the one shown, in place of a member's Changeset.
    pub fn stack_changeset_selected(&self) -> bool {
        self.whole_stack.selected && self.offers_stack_changeset()
    }

    /// The Stack Changeset, or why it couldn't be fetched, while it is the one shown. `None`
    /// otherwise, and until it is fetched.
    pub fn stack_changeset(&self) -> Option<Result<&Changeset, &str>> {
        if !self.stack_changeset_selected() || self.whole_stack.fetched_for.is_none() {
            return None;
        }
        Some(self.whole_stack.error.as_deref().map_or(Ok(&self.whole_stack.changeset), Err))
    }

    /// What the Stack Changeset is fetched for now; `None` unless the Stack offers it.
    fn stack_fetch(&self) -> Option<Fetch> {
        let members: Vec<PullRequest> =
            self.stack().into_iter().filter(|m| m.in_stack).map(|m| m.pull_request).collect();
        let (bottom, top) = (members.first()?, members.last()?);
        if members.len() < 2 {
            return None;
        }
        let key = members.iter().map(|p| (p.id.clone(), p.base_sha.clone(), p.head_sha.clone())).collect();
        Some(Fetch { members: key, bottom: bottom.clone(), head: top.head_sha.clone() })
    }

    /// Shows the member selected again, if the Stack Changeset was shown. Whether it was.
    pub(super) fn deselect_stack_changeset(&mut self) -> bool {
        let was = std::mem::replace(&mut self.whole_stack.selected, false);
        if was {
            // The diff of another Changeset: the UI must rebuild it.
            self.changeset_rev += 1;
        }
        was
    }

    /// Takes the outcome of fetching the Stack Changeset for `members`.
    fn store_stack_changeset(&mut self, members: Members, fetched: Result<Changeset, String>) {
        let whole = &mut self.whole_stack;
        whole.fetched_for = Some(members);
        match fetched {
            Ok(changeset) => (whole.changeset, whole.error) = (changeset, None),
            Err(error) => (whole.changeset, whole.error) = (Changeset::default(), Some(error)),
        }
        self.changeset_rev += 1;
    }
}

impl Service {
    /// Human: shows the Stack Changeset of the Session's Stack, fetched from the Forge the first
    /// time and again when the Stack's members or their heads changed since. A failure to fetch
    /// is kept in the Session's view in its place, and is not an error here. Blocks on the Forge.
    pub fn select_stack_changeset(&self, id: &SessionId) -> Result<(), Error> {
        let fetch = {
            let inner = self.lock();
            let view = &inner.sessions[inner.position(id)?].view;
            view.stack_fetch().ok_or(Error::NoStackChangeset)?
        };
        self.fetch_stack_changeset(id, fetch, true)
    }

    /// Fetches the Stack Changeset again if it is shown and its Stack changed, or takes it off
    /// the Window when the Stack no longer offers it. Blocks on the Forge.
    pub(super) fn refresh_stack_changeset(&self, id: &SessionId) {
        let fetch = {
            let mut inner = self.lock();
            let Ok(at) = inner.position(id) else { return };
            let view = &mut inner.sessions[at].view;
            if !view.whole_stack.selected {
                return;
            }
            let Some(fetch) = view.stack_fetch() else {
                view.deselect_stack_changeset();
                inner.notify();
                return;
            };
            fetch
        };
        // A failure shows in place of the Changeset; there is nobody to return it to.
        let _ = self.fetch_stack_changeset(id, fetch, false);
    }

    /// Fetches the Stack Changeset for `fetch` unless the Session has it already, and keeps it.
    /// With `select`, shows it.
    fn fetch_stack_changeset(&self, id: &SessionId, fetch: Fetch, select: bool) -> Result<(), Error> {
        let have = {
            let inner = self.lock();
            let whole = &inner.sessions[inner.position(id)?].view.whole_stack;
            whole.fetched_for.as_ref() == Some(&fetch.members) && (whole.error.is_none() || !select)
        };
        // Not holding the lock: the Forge may take a while.
        let fetched = (!have).then(|| match self.forge_for(&fetch.bottom) {
            Some(forge) => forge
                .changeset_between(&fetch.bottom.id.repo, &fetch.bottom.base_sha, &fetch.head)
                .map_err(|e| e.to_string()),
            None => Err(format!("no Forge serves {}", fetch.bottom.id)),
        });
        let mut inner = self.lock();
        let at = inner.position(id)?;
        let view = &mut inner.sessions[at].view;
        let shown_before = view.whole_stack.selected;
        if let Some(fetched) = fetched {
            view.store_stack_changeset(fetch.members, fetched);
        }
        if select && !shown_before {
            view.whole_stack.selected = true;
            view.changeset_rev += 1;
        }
        inner.notify();
        Ok(())
    }
}
