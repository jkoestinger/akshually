//! Stacks: a Session of Pull Requests holds one Review per Pull Request of a Stack, detected from
//! the Forge and followed as it changes (ADR 0004).
//!
//! A Session shows one member at a time. The [`SessionView`] fields that make up a Review (its
//! Changeset, Threads, Changeset view, ticks and pushes on offer) hold the selected member's;
//! the other members' wait in [`Member`]s, and switching swaps them. So everything that works on
//! "the Session's Threads" works on the member shown, and `Session::at_member` reaches another.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::mem;

use serde::{Deserialize, Serialize};

use super::{ChangesetView, Service, Session, SessionId, SessionView, Source};
use crate::forge::{ForgeAdapter, ForgeError, PullRequest, PullRequestRef, PullRequestUpdate};
use crate::thread::{Thread, ThreadId};
use crate::{Changeset, Error, Verdict};

/// What a member's Publish sent to its Pull Request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishedReview {
    pub verdict: Verdict,
    /// The review's page on the Forge.
    pub url: String,
}

/// A member of a Stack as the Session keeps it: its Review, parked while another member is
/// shown. The selected member's own slot is stale (its Review is in the [`SessionView`]'s own
/// fields), apart from `in_stack`. Read members through [`SessionView::stack`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Member {
    pub(super) pull_request: PullRequest,
    pub(super) changeset: Changeset,
    pub(super) threads: Vec<Thread>,
    pub(super) view: ChangesetView,
    pub(super) viewed: BTreeSet<String>,
    pub(super) pull_request_update: Option<PullRequestUpdate>,
    pub(super) last_round: Option<Changeset>,
    pub(super) since_last_round: Option<Changeset>,
    pub(super) published: Option<PublishedReview>,
    /// Whether the Pull Request is still part of the Stack: not merged, closed or retargeted
    /// away. One that left stays while it holds Drafts or was Published here.
    pub(super) in_stack: bool,
}

impl Member {
    /// A member with the Pull Request's Changeset and Threads.
    pub(super) fn new(pull_request: PullRequest, changeset: Changeset, threads: Vec<Thread>) -> Self {
        Member { pull_request, changeset, threads, in_stack: true, ..Member::default() }
    }

    /// Whether it holds anything the human wrote or was Published, which keeps it in the Session
    /// after it left the Stack.
    fn worth_keeping(&self) -> bool {
        self.published.is_some() || self.threads.iter().flat_map(|t| &t.comments).any(|c| c.is_draft())
    }
}

/// One member of a Stack, as the picker lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberView {
    pub pull_request: PullRequest,
    /// How many Drafts its Review holds.
    pub drafts: usize,
    /// Its Verdict and the review on the Forge, once Published.
    pub published: Option<PublishedReview>,
    /// It is still part of the Stack; `false` for a member that was merged, closed or retargeted
    /// away since the Session opened, and is kept for its Drafts or its Publish.
    pub in_stack: bool,
    /// It is the member the Session shows.
    pub selected: bool,
}

impl SessionView {
    /// The members of the Session's Stack, bottom to top; empty for local work. A Pull Request
    /// Session always has at least one.
    pub fn stack(&self) -> Vec<MemberView> {
        if self.members.is_empty() {
            let Some(pull_request) = self.pull_request() else { return Vec::new() };
            return vec![MemberView {
                pull_request: pull_request.clone(),
                drafts: self.drafts(),
                published: self.published.clone(),
                in_stack: true,
                selected: true,
            }];
        }
        let parked = |m: &Member| MemberView {
            pull_request: m.pull_request.clone(),
            drafts: m.threads.iter().flat_map(|t| &t.comments).filter(|c| c.is_draft()).count(),
            published: m.published.clone(),
            in_stack: m.in_stack,
            selected: false,
        };
        let shown = |m: &Member| MemberView {
            pull_request: self.pull_request().cloned().unwrap_or_else(|| m.pull_request.clone()),
            drafts: self.drafts(),
            published: self.published.clone(),
            in_stack: m.in_stack,
            selected: true,
        };
        let selected = self.selected;
        self.members.iter().enumerate().map(|(at, m)| if at == selected { shown(m) } else { parked(m) }).collect()
    }

    /// A Pull Request Session saved before Stacks holds no members: it is a Stack of one.
    pub(super) fn restore_stack(&mut self) {
        let Source::PullRequest(pull_request) = &self.source else { return };
        if self.members.is_empty() {
            self.members = vec![Member::new(pull_request.clone(), Changeset::default(), Vec::new())];
            self.selected = 0;
        }
    }

    /// Whether `pr` is one of the Session's members.
    pub fn has_member(&self, pr: &PullRequestRef) -> bool {
        self.member_position(pr).is_some()
    }

    /// The position of `pr` among the members; `None` if it isn't one.
    pub(super) fn member_position(&self, pr: &PullRequestRef) -> Option<usize> {
        if self.members.is_empty() {
            return self.pull_request().filter(|p| &p.id == pr).map(|_| 0);
        }
        (0..self.members.len()).find(|&at| &self.member_pull_request(at).id == pr)
    }

    /// The Pull Request of the member at `at`, which is in range.
    fn member_pull_request(&self, at: usize) -> &PullRequest {
        match self.pull_request() {
            Some(shown) if at == self.selected => shown,
            _ => &self.members[at].pull_request,
        }
    }

    /// The Pull Request Session opened on, or else the one shown.
    pub(super) fn opened_pull_request(&self) -> Option<&PullRequest> {
        let opened = self.opened.as_ref().and_then(|o| self.member_position(o));
        opened.map(|at| self.member_pull_request(at)).or_else(|| self.pull_request())
    }

    /// Every Thread of the Session, whichever member's Review it is in.
    pub(super) fn all_threads(&self) -> impl Iterator<Item = &Thread> {
        let shown = self.selected;
        let parked = self.members.iter().enumerate().filter(move |(at, _)| *at != shown).flat_map(|(_, m)| &m.threads);
        self.threads.iter().chain(parked)
    }

    /// `all_threads`, to change.
    pub(super) fn all_threads_mut(&mut self) -> impl Iterator<Item = &mut Thread> {
        let shown = self.selected;
        let parked =
            self.members.iter_mut().enumerate().filter(move |(at, _)| *at != shown).flat_map(|(_, m)| &mut m.threads);
        self.threads.iter_mut().chain(parked)
    }

    /// The position of the member whose Review holds `thread`.
    pub(super) fn member_of(&self, thread: &ThreadId) -> Option<usize> {
        if self.threads.iter().any(|t| &t.id == thread) {
            return Some(self.selected);
        }
        self.members.iter().position(|m| m.threads.iter().any(|t| &t.id == thread))
    }

    /// Whether the member shown was Published, and takes no more Drafts.
    pub(super) fn is_published(&self) -> bool {
        self.published.is_some()
    }

    /// What Publish sent for `pr`, when it did.
    pub(super) fn published_review(&self, pr: &PullRequestRef) -> Option<PublishedReview> {
        self.stack().into_iter().find(|m| &m.pull_request.id == pr)?.published
    }

    /// The Review that ends the Session as Published: the topmost member's that was Published.
    pub(super) fn last_published(&self) -> Option<PublishedReview> {
        self.stack().into_iter().rev().find_map(|m| m.published)
    }

    /// Whether every member still in the Stack was Published, and at least one was.
    pub(super) fn all_published(&self) -> bool {
        let stack = self.stack();
        stack.iter().any(|m| m.published.is_some())
            && stack.iter().filter(|m| m.in_stack).all(|m| m.published.is_some())
    }

    /// Exchanges the Review in the Session's own fields with the one parked at `at`.
    pub(super) fn swap_in(&mut self, at: usize) {
        let mut member = mem::take(&mut self.members[at]);
        if let Source::PullRequest(pull_request) = &mut self.source {
            mem::swap(pull_request, &mut member.pull_request);
        }
        mem::swap(&mut self.changeset, &mut member.changeset);
        mem::swap(&mut self.threads, &mut member.threads);
        mem::swap(&mut self.view, &mut member.view);
        mem::swap(&mut self.viewed, &mut member.viewed);
        mem::swap(&mut self.pull_request_update, &mut member.pull_request_update);
        mem::swap(&mut self.last_round, &mut member.last_round);
        mem::swap(&mut self.since_last_round, &mut member.since_last_round);
        mem::swap(&mut self.published, &mut member.published);
        self.members[at] = member;
    }

    /// Shows the member at `to`, which is in range.
    pub(super) fn select(&mut self, to: usize) {
        if to == self.selected {
            return;
        }
        self.swap_in(self.selected);
        self.swap_in(to);
        self.selected = to;
        // The diff of another Review: the UI must rebuild it even if it reads the same.
        self.changeset_rev += 1;
    }
}

impl Session {
    /// Runs `f` on the Session as if the member at `at` were shown, without changing what is.
    pub(super) fn at_member<T>(&mut self, at: usize, f: impl FnOnce(&mut Session) -> T) -> T {
        if self.view.members.is_empty() || at == self.view.selected {
            return f(self);
        }
        self.view.swap_in(at);
        let result = f(self);
        self.view.swap_in(at);
        result
    }

    /// Takes the Stack as the Forge has it now: `stack` bottom to top, with `fetched` the Review
    /// material of the Pull Requests that are new to the Session. A member no longer in `stack`
    /// stays, marked, while it holds Drafts or was Published; otherwise it goes.
    ///
    /// Members that left come first: a Stack merges from the bottom, so they sit where they were.
    pub(super) fn follow_stack(
        &mut self,
        stack: &[PullRequest],
        mut fetched: HashMap<PullRequestRef, Fetched>,
    ) -> bool {
        let Some(shown) = self.view.pull_request().map(|p| p.id.clone()) else { return false };
        if self.view.members.is_empty() {
            return false;
        }
        let before: Vec<_> = self.view.stack().into_iter().map(|m| (m.pull_request.id, m.in_stack)).collect();
        let wanted = |pr: &PullRequest| self.view.has_member(&pr.id) || fetched.contains_key(&pr.id);
        let after: Vec<_> = stack.iter().filter(|p| wanted(p)).map(|p| (p.id.clone(), true)).collect();
        let kept_before: Vec<_> = before.iter().filter(|(_, in_stack)| *in_stack).cloned().collect();
        if after == kept_before {
            return false;
        }
        // All the Reviews parked, so they can be put in a new order.
        self.view.swap_in(self.view.selected);
        let old = mem::take(&mut self.view.members);
        let (mut left, mut staying): (Vec<Member>, HashMap<PullRequestRef, Member>) = (Vec::new(), HashMap::new());
        let mut gone_shown = None;
        for mut member in old {
            if stack.iter().any(|p| p.id == member.pull_request.id) {
                staying.insert(member.pull_request.id.clone(), member);
            } else if member.worth_keeping() {
                member.in_stack = false;
                left.push(member);
            } else if member.pull_request.id == shown {
                gone_shown = Some(member);
            }
        }
        let mut members = left;
        for pr in stack {
            if let Some(mut member) = staying.remove(&pr.id) {
                member.pull_request.title.clone_from(&pr.title);
                member.pull_request.base_branch.clone_from(&pr.base_branch);
                member.in_stack = true;
                members.push(member);
            } else if let Some(new) = fetched.remove(&pr.id) {
                let threads = new.threads.into_iter().map(|thread| self.number(thread)).collect();
                members.push(Member::new(pr.clone(), new.changeset, threads));
            }
        }
        // Never an empty Session: the member shown stays, marked, when it was all that was left.
        if members.is_empty()
            && let Some(mut member) = gone_shown
        {
            member.in_stack = false;
            members.push(member);
        }
        let selected = members
            .iter()
            .position(|m| m.pull_request.id == shown)
            .or_else(|| members.iter().position(|m| m.in_stack))
            .unwrap_or(0);
        self.view.members = members;
        self.view.selected = selected;
        self.view.swap_in(selected);
        self.view.changeset_rev += 1;
        true
    }

    /// Gives `thread`, new to the Session, the next Thread id.
    pub(super) fn number(&mut self, thread: Thread) -> Thread {
        self.threads_opened += 1;
        Thread { id: ThreadId::new(format!("T{}", self.threads_opened)), ..thread }
    }
}

/// What a Pull Request new to a Session brings: its Changeset and the Threads on it (not yet
/// numbered).
pub(super) struct Fetched {
    pub(super) changeset: Changeset,
    pub(super) threads: Vec<Thread>,
}

impl Service {
    /// Human: shows the member `pr` of the Session's Stack: its Changeset, Threads, Drafts,
    /// Changeset view and Verdict. What the other members hold stays as it was.
    pub fn select_member(&self, id: &SessionId, pr: &PullRequestRef) -> Result<(), Error> {
        let mut inner = self.lock();
        let at = inner.position(id)?;
        let view = &mut inner.sessions[at].view;
        let to = view.member_position(pr).ok_or_else(|| Error::NotInStack(pr.to_string()))?;
        if to != view.selected {
            view.select(to);
            inner.notify();
        }
        Ok(())
    }

    /// Looks for what changed in the Stack of the Session `id` on its Forge, and takes it: new
    /// members join, and those that left stay or go (`Session::follow_stack`). A Pull Request
    /// that has a Session of its own is never taken. A Forge that can't be reached changes
    /// nothing this time. Blocks on the Forge.
    pub(super) fn follow_stack(&self, id: &SessionId) {
        let (members, owned) = {
            let inner = self.lock();
            let Ok(at) = inner.position(id) else { return };
            let members = inner.sessions[at].view.stack();
            let owned: HashSet<PullRequestRef> = inner
                .sessions
                .iter()
                .filter(|s| &s.view.id != id)
                .flat_map(|s| s.view.stack().into_iter().map(|m| m.pull_request.id))
                .collect();
            (members, owned)
        };
        // Not holding the lock: the Forge may take a while.
        let Some(first) = members.iter().find(|m| m.in_stack) else { return };
        let Some(forge) = self.forge_for(&first.pull_request) else { return };
        let Ok(stack) = current_stack(forge, &members) else { return };
        let taken = |pr: &PullRequest| !owned.contains(&pr.id);
        let stack: Vec<PullRequest> = stack.into_iter().filter(taken).collect();
        let mut fetched = HashMap::new();
        for pr in stack.iter().filter(|p| !members.iter().any(|m| m.pull_request.id == p.id)) {
            let (Ok(changeset), Ok(remote)) = (forge.changeset(pr), forge.threads(pr)) else { return };
            let threads = remote.into_iter().map(|t| super::pull_request::existing_thread(t, &changeset)).collect();
            fetched.insert(pr.id.clone(), Fetched { changeset, threads });
        }
        let mut inner = self.lock();
        let Ok(at) = inner.position(id) else { return };
        // Another Session may have taken one of them meanwhile.
        let stack: Vec<PullRequest> = stack
            .into_iter()
            .filter(|p| {
                let mine = inner.sessions[at].view.has_member(&p.id);
                mine || !inner.sessions.iter().any(|s| s.view.has_member(&p.id))
            })
            .collect();
        if inner.sessions[at].follow_stack(&stack, fetched) {
            inner.notify();
        }
    }
}

/// The Stack `members` make now, bottom to top: found from the first one still open, or empty
/// when none is. `Err` when the Forge can't say.
///
/// A member is found by its head branch, a narrow question: the branch it asks to merge into may
/// be one with more open Pull Requests than a Forge lists at once.
fn current_stack(forge: &dyn ForgeAdapter, members: &[MemberView]) -> Result<Vec<PullRequest>, ForgeError> {
    // The member shown first: it is the one the human is looking at.
    let order = members.iter().filter(|m| m.in_stack && m.selected).chain(members.iter().filter(|m| m.in_stack));
    // The Forge finds a head branch of this repository, never a fork's.
    let mut asked = false;
    for member in order.filter(|m| !m.pull_request.from_fork) {
        let pull_request = &member.pull_request;
        asked = true;
        let named = forge.open_with_head_branch(&pull_request.id.repo, &pull_request.head_branch)?;
        // Open and still asking to merge where it did; a member saved before Stacks doesn't know.
        let asking = |p: &PullRequest| pull_request.base_branch.is_empty() || p.base_branch == pull_request.base_branch;
        if let Some(open) = named.into_iter().find(|p| p.id == pull_request.id && asking(p)) {
            return detect_stack(forge, &open);
        }
    }
    if !asked {
        let host = members.first().map_or_else(String::new, |m| m.pull_request.id.host.clone());
        return Err(ForgeError::Unsupported { host, what: "tell whether a Pull Request from a fork is still open" });
    }
    Ok(Vec::new())
}

/// The Stack of `opened`, bottom to top: its ancestors (the open Pull Request, in the same
/// repository, whose head branch it asks to merge into, and so on down), and its descendants for
/// as long as exactly one open Pull Request asks to merge into the top's head branch.
///
/// A head branch in a fork is no branch of the repository: no Pull Request is stacked on it, and
/// a fork's Pull Request is never an ancestor through its branch's name.
pub(super) fn detect_stack(forge: &dyn ForgeAdapter, opened: &PullRequest) -> Result<Vec<PullRequest>, ForgeError> {
    let same_repo = |a: &PullRequest, b: &PullRequest| a.id.host == b.id.host && a.id.repo == b.id.repo;
    // Branch names can make a cycle (A into B's branch, B into A's).
    let mut seen = HashSet::from([opened.id.clone()]);
    let mut below = Vec::new();
    let mut at = opened.clone();
    while !at.base_branch.is_empty() {
        let candidates = or_none(forge.open_with_head_branch(&at.id.repo, &at.base_branch))?;
        let parent = candidates
            .into_iter()
            .filter(|p| !p.from_fork && same_repo(p, &at) && !seen.contains(&p.id))
            .min_by_key(|p| p.id.number);
        let Some(parent) = parent else { break };
        seen.insert(parent.id.clone());
        below.push(parent.clone());
        at = parent;
    }
    below.reverse();
    let mut stack = below;
    stack.push(opened.clone());
    let mut top = opened.clone();
    while !top.from_fork {
        let asking = or_none(forge.open_with_target_branch(&top.id.repo, &top.head_branch))?;
        let mut above: Vec<PullRequest> =
            asking.into_iter().filter(|p| same_repo(p, &top) && !seen.contains(&p.id)).collect();
        // A line stops below two Pull Requests on the same one.
        let (1, Some(next)) = (above.len(), above.pop()) else { break };
        seen.insert(next.id.clone());
        stack.push(next.clone());
        top = next;
    }
    Ok(stack)
}

/// A Forge that can't tell which Pull Requests sit on a branch has none: every Pull Request is a
/// Stack of one.
fn or_none(found: Result<Vec<PullRequest>, ForgeError>) -> Result<Vec<PullRequest>, ForgeError> {
    match found {
        Err(ForgeError::Unsupported { .. }) => Ok(Vec::new()),
        found => found,
    }
}

/// The members of `stack` that can be one Session with `opened`: the run around it that no other
/// Session holds a Pull Request of.
pub(super) fn free_run(
    stack: Vec<PullRequest>,
    opened: &PullRequestRef,
    owned: impl Fn(&PullRequestRef) -> bool,
) -> Vec<PullRequest> {
    let Some(at) = stack.iter().position(|p| &p.id == opened) else { return stack };
    let first = (0..at).rev().find(|&i| owned(&stack[i].id)).map_or(0, |i| i + 1);
    let last = (at + 1..stack.len()).find(|&i| owned(&stack[i].id)).unwrap_or(stack.len());
    stack[first..last].to_vec()
}
