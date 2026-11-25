//! The Stack Changeset: everything a Stack changes as one Changeset, from the bottom Member's
//! base to the top Member's head, for reading only (ADR 0004). It takes no Threads: a Forge
//! anchors a comment in one Pull Request's own diff.

use super::{Service, SessionId, SessionView};
use crate::forge::{Comparison, PullRequest, PullRequestRef};
use crate::{Changeset, Error};

/// What a Stack Changeset was fetched for: each Member still in the Stack, with the commits it
/// covers. It is fetched again when this changes.
#[derive(Clone, Debug, PartialEq, Eq)]
struct StackCommits(Vec<MemberCommits>);

/// The base and head commit of one Member, as far as the Session knows: the head of a push it
/// hasn't reloaded yet counts.
#[derive(Clone, Debug, PartialEq, Eq)]
struct MemberCommits {
    pull_request: PullRequestRef,
    base_sha: String,
    head_sha: String,
}

/// A Session's Stack Changeset, and whether it is the one shown. Not saved: it is fetched again
/// when first selected after a restart.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StackChangeset {
    /// The Window shows it in place of the selected Member's Changeset (see
    /// [`SessionView::stack_changeset_selected`]).
    selected: bool,
    /// The Stack Changeset fetched; empty until then, and after a failure.
    changeset: Changeset,
    /// Why fetching it failed, shown in its place.
    error: Option<String>,
    /// Why what was fetched is only part of the Stack, when the Forge cut it short.
    cut_off: Option<String>,
    /// What it was fetched for; `None` before the first fetch.
    fetched_for: Option<StackCommits>,
}

impl StackChangeset {
    /// The Changeset to show while it is selected.
    pub(super) fn shown(&self) -> &Changeset {
        &self.changeset
    }
}

/// The Stack as a Stack Changeset is fetched for it.
struct Fetch {
    commits: StackCommits,
    /// The bottom Member, whose base the Changeset starts at and whose Forge serves it.
    bottom: PullRequest,
    /// The head commit of the top Member, where it ends.
    head: String,
}

impl SessionView {
    /// How many Members are still part of the Stack: those that left it are not counted.
    pub fn members_in_stack(&self) -> usize {
        self.stack().iter().filter(|m| m.in_stack).count()
    }

    /// Whether the Session offers the Stack Changeset: its Stack has two or more Members.
    pub fn offers_stack_changeset(&self) -> bool {
        self.members_in_stack() >= 2
    }

    /// Whether the Stack Changeset is the one shown, in place of a Member's Changeset.
    pub fn stack_changeset_selected(&self) -> bool {
        self.stack_changeset.selected && self.offers_stack_changeset()
    }

    /// The Stack Changeset, or why it couldn't be fetched, while it is the one shown. `None`
    /// otherwise, and until it is fetched.
    pub fn stack_changeset(&self) -> Option<Result<&Changeset, &str>> {
        if !self.stack_changeset_selected() || self.stack_changeset.fetched_for.is_none() {
            return None;
        }
        Some(self.stack_changeset.error.as_deref().map_or(Ok(&self.stack_changeset.changeset), Err))
    }

    /// Why the Stack Changeset shown is only part of what the Stack changes, when the Forge cut
    /// it short: the Window says so instead of presenting it as complete.
    pub fn stack_changeset_cut_off(&self) -> Option<&str> {
        self.stack_changeset.cut_off.as_deref().filter(|_| self.stack_changeset_selected())
    }

    /// What the Stack Changeset is fetched for now; `None` unless the Stack offers it. A push the
    /// Session hasn't reloaded yet counts, so a check that finds one fetches it again.
    fn stack_fetch(&self) -> Option<Fetch> {
        let stack = self.stack();
        let members: Vec<&PullRequest> = stack.iter().filter(|m| m.in_stack).map(|m| m.latest()).collect();
        let (bottom, top) = (members.first()?, members.last()?);
        if members.len() < 2 {
            return None;
        }
        let commits = members
            .iter()
            .map(|p| MemberCommits {
                pull_request: p.id.clone(),
                base_sha: p.base_sha.clone(),
                head_sha: p.head_sha.clone(),
            })
            .collect();
        Some(Fetch { commits: StackCommits(commits), bottom: (*bottom).clone(), head: top.head_sha.clone() })
    }

    /// Shows the Member selected again, if the Stack Changeset was shown. Whether it was.
    pub(super) fn deselect_stack_changeset(&mut self) -> bool {
        let was = std::mem::replace(&mut self.stack_changeset.selected, false);
        if was {
            // The diff of another Changeset: the UI must rebuild it.
            self.changeset_rev += 1;
        }
        was
    }

    /// Takes the outcome of fetching the Stack Changeset for `commits`.
    fn store_stack_changeset(&mut self, commits: StackCommits, fetched: Result<Comparison, String>) {
        let whole = &mut self.stack_changeset;
        whole.fetched_for = Some(commits);
        match fetched {
            Ok(Comparison { changeset, cut_off }) => {
                (whole.changeset, whole.error, whole.cut_off) = (changeset, None, cut_off)
            }
            Err(error) => (whole.changeset, whole.error, whole.cut_off) = (Changeset::default(), Some(error), None),
        }
        self.changeset_rev += 1;
    }
}

impl Service {
    /// Human: shows the Stack Changeset of the Session's Stack, fetched from the Forge the first
    /// time and again when the Stack's Members or their heads changed since. A failure to fetch
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
            if !view.stack_changeset.selected {
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
            let whole = &inner.sessions[inner.position(id)?].view.stack_changeset;
            whole.fetched_for.as_ref() == Some(&fetch.commits) && (whole.error.is_none() || !select)
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
        let shown_before = view.stack_changeset.selected;
        if let Some(fetched) = fetched {
            view.store_stack_changeset(fetch.commits, fetched);
        }
        if select && !shown_before {
            view.stack_changeset.selected = true;
            view.changeset_rev += 1;
        }
        inner.notify();
        Ok(())
    }
}
