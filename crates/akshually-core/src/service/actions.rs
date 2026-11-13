//! What the human can do in a Session now: the one table both the UI (which offers these
//! actions) and the service (which refuses every other) go by (#38).

use super::{Role, SessionView, Source, Turn};
use crate::Error;
use crate::thread::{Comment, Thread, ThreadId};

/// What the human can do in a Session now, worked out from its Source, agent role, Turn and
/// whether it has ended. The service refuses every action this doesn't offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionActions {
    /// The Verdict, or what stands in for it; `None` when neither applies: a Pull Request takes
    /// its Verdict at Publish, a co-review of local work has none, and an ended Session is done.
    pub verdict: Option<VerdictAction>,
    /// How the human ends the Session.
    pub end: EndAction,
    /// Handing the next Round over; `None` while the Session has no agent to hand it to.
    pub hand_over: Option<HandOver>,
    /// What the human can do in each Thread, in the Session's order.
    pub threads: Vec<ThreadActions>,
}

impl SessionActions {
    /// What the human can do in `thread`; `None` for a Thread the Session doesn't have.
    pub fn thread(&self, thread: &ThreadId) -> Option<&ThreadActions> {
        self.threads.iter().find(|t| &t.thread == thread)
    }
}

/// The Verdict action a Session of local work offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerdictAction {
    /// Approve the agent's local work, in the human's Turn.
    Approve,
    /// During the agent's Turn the Verdict waits: take the Turn back first.
    TakeTurnBack,
}

/// How the human ends a Session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndAction {
    /// Closing is a normal end: the Session has its outcome already, or it's a co-review of
    /// local work, which has no Verdict.
    Close,
    /// Ending now loses something, so the human confirms first: local work's Author is told
    /// it was Abandoned, and a Pull Request not Published yet loses its Drafts (and its
    /// co-Reviewer is told it was Abandoned).
    Abandon,
}

/// Handing the next Round over to the Session's agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HandOver {
    /// How many Comments and reopened Threads the Round would carry (`to_hand_over`).
    pub items: usize,
    /// Whether it can be handed over now: in the human's Turn, before the Session ended, with
    /// something to carry.
    pub offered: bool,
}

/// What the human can do in one Thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadActions {
    pub thread: ThreadId,
    /// Reply in it: once the agent (or the Forge) has it, or to question an agent's Draft.
    pub reply: bool,
    /// Mark it Resolved: an open Thread the agent (or the Forge) has.
    pub resolve: bool,
    /// Reopen it, while it's Resolved.
    pub reopen: bool,
    /// Keep, edit or discard the co-Reviewer's Draft that opens it.
    pub agent_draft: bool,
}

impl SessionView {
    /// What the human can do in the Session now. Worked out on each call, not saved.
    pub fn actions(&self) -> SessionActions {
        let ended = self.ended.is_some();
        let yours = self.turn == Turn::Human;
        let verdict = match &self.source {
            _ if ended => None,
            Source::PullRequest(_) => None,
            Source::Local { .. } if !yours => Some(VerdictAction::TakeTurnBack),
            Source::Local { .. } if self.agent == Some(Role::CoReviewer) => None,
            Source::Local { .. } => Some(VerdictAction::Approve),
        };
        let end = match &self.source {
            _ if ended => EndAction::Close,
            Source::Local { .. } if self.agent == Some(Role::CoReviewer) => EndAction::Close,
            _ => EndAction::Abandon,
        };
        let hand_over = self.agent.map(|_| {
            let items = self.to_hand_over();
            HandOver { items, offered: items > 0 && yours && !ended }
        });
        let threads = self.threads.iter().map(|t| thread_actions(t, ended)).collect();
        SessionActions { verdict, end, hand_over, threads }
    }

    // Why the service refuses an action `actions()` doesn't offer, next to the table so the
    // reasons follow its rules.

    /// Refuses approving unless the Session offers it.
    pub(super) fn check_approve(&self) -> Result<(), Error> {
        if self.actions().verdict == Some(VerdictAction::Approve) {
            return Ok(());
        }
        Err(match self.pull_request() {
            _ if self.ended.is_some() => Error::Ended,
            Some(_) => Error::InvalidRequest("a Pull Request is approved by publishing the Verdict".into()),
            None if self.turn != Turn::Human => Error::NotYourTurn,
            None => Error::InvalidRequest("a co-review of local work has no Verdict: close it".into()),
        })
    }

    /// Refuses taking the Turn back unless the Session offers it.
    pub(super) fn check_take_turn_back(&self) -> Result<(), Error> {
        if self.actions().verdict == Some(VerdictAction::TakeTurnBack) {
            return Ok(());
        }
        Err(match self {
            _ if self.ended.is_some() => Error::Ended,
            _ if self.turn == Turn::Human => Error::AlreadyYourTurn,
            _ => Error::InvalidRequest(
                "a Pull Request's co-Reviewer gives the Turn back when it next asks for review".into(),
            ),
        })
    }

    /// Refuses handing the Round over unless the Session offers it.
    pub(super) fn check_hand_over(&self) -> Result<(), Error> {
        if self.actions().hand_over.is_some_and(|h| h.offered) {
            return Ok(());
        }
        Err(match self {
            _ if self.ended.is_some() => Error::Ended,
            _ if self.turn != Turn::Human => Error::NotYourTurn,
            _ => Error::NoDrafts,
        })
    }

    /// What the Session offers in `thread`; refused outright for a Thread it doesn't have, or
    /// once it has ended.
    pub(super) fn thread_offered(&self, thread: &ThreadId) -> Result<ThreadActions, Error> {
        let found =
            self.threads.iter().find(|t| &t.id == thread).ok_or_else(|| Error::UnknownThread(thread.clone()))?;
        if self.ended.is_some() {
            return Err(Error::Ended);
        }
        Ok(thread_actions(found, false))
    }
}

/// What the human can do in `thread`, nothing once the Session has `ended`.
fn thread_actions(thread: &Thread, ended: bool) -> ThreadActions {
    let open = !ended;
    let agent_draft = thread.comments.first().is_some_and(Comment::is_agent_draft);
    let sent = is_sent(thread);
    ThreadActions {
        thread: thread.id.clone(),
        reply: open && (sent || agent_draft),
        resolve: open && sent && thread.resolved.is_none(),
        reopen: open && thread.resolved.is_some(),
        agent_draft: open && agent_draft,
    }
}

/// Whether the agent (or the Forge) has `thread`: a Thread still all Drafts goes out whole with
/// the next Round, so there's nothing to reply to or resolve yet.
fn is_sent(thread: &Thread) -> bool {
    thread.comments.iter().any(|c| !c.is_draft())
}
