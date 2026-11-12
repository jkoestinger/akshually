//! Conversation in Threads: replies and resolutions from either side.

use std::path::PathBuf;

use super::{Service, Session, SourceRef, ThreadActions, ThreadRef};
use crate::forge::PullRequestRef;
use crate::thread::{Comment, Party, Thread, ThreadId};
use crate::{Error, git};

/// A Session's identity: the repository and branch of local work, or the Pull Request (with
/// the URL it was asked by, for errors).
enum SessionKey {
    Local { repo: PathBuf, branch: String },
    PullRequest(PullRequestRef, String),
}

impl SessionKey {
    /// The position of its Session in `sessions`.
    fn find(self, sessions: &[Session]) -> Result<usize, Error> {
        let found = sessions.iter().position(|s| match &self {
            SessionKey::Local { repo, branch } => s.view.is_local(repo, branch),
            SessionKey::PullRequest(pr, _) => s.view.pull_request().is_some_and(|p| &p.id == pr),
        });
        if let Some(at) = found {
            return Ok(at);
        }
        Err(match self {
            SessionKey::Local { repo, branch } => Error::NoSessionHere { repo, branch },
            SessionKey::PullRequest(_, url) => Error::NoPullRequestSession(url),
        })
    }
}

impl Service {
    /// Agent: replies to `thread` in the Session of `source`. The human sees it at once; it
    /// counts towards the Round being drafted.
    pub fn reply(&self, source: impl Into<SourceRef>, thread: &ThreadId, body: impl Into<String>) -> Result<(), Error> {
        let body = body.into();
        if body.trim().is_empty() {
            return Err(Error::EmptyComment);
        }
        self.agent_thread(&source.into(), thread, |round, thread| {
            thread.comments.push(Comment { author: Party::Agent, body, round: Some(round), suggestion: None });
        })
    }

    /// Agent: marks `thread` Resolved in the Session of `source`.
    pub fn resolve(&self, source: impl Into<SourceRef>, thread: &ThreadId) -> Result<(), Error> {
        self.agent_thread(&source.into(), thread, |_, thread| resolve(thread, Party::Agent))
    }

    /// Human: replies in `thread` with `body` (markdown) as a Draft, which goes out with the
    /// next Round under that Thread. Refused while the Thread is still all Drafts, unless a
    /// co-Reviewer's Draft opens it (the reply questions that Draft).
    pub fn reply_in_thread(&self, thread: &ThreadRef, body: impl Into<String>) -> Result<(), Error> {
        let body = body.into();
        if body.trim().is_empty() {
            return Err(Error::EmptyComment);
        }
        let offered = |offered: &ThreadActions| if offered.reply { Ok(()) } else { Err(Error::NotSent) };
        self.human_thread(thread, offered, |t| {
            t.comments.push(Comment { author: Party::Human, body, round: None, suggestion: None });
        })
    }

    /// Human: marks `thread` Resolved, whoever opened it. Refused while it's still all Drafts,
    /// and while it's Resolved already.
    pub fn resolve_thread(&self, thread: &ThreadRef) -> Result<(), Error> {
        let offered = |offered: &ThreadActions| match offered {
            ThreadActions { resolve: true, .. } => Ok(()),
            ThreadActions { reopen: true, thread, .. } => {
                Err(Error::InvalidRequest(format!("{thread} is Resolved already")))
            }
            _ => Err(Error::NotSent),
        };
        self.human_thread(thread, offered, |t| resolve(t, Party::Human))
    }

    /// Human: reopens a Resolved `thread`, whoever resolved it. The next Round tells the agent.
    pub fn reopen_thread(&self, thread: &ThreadRef) -> Result<(), Error> {
        let offered = |offered: &ThreadActions| match offered.reopen {
            true => Ok(()),
            false => Err(Error::InvalidRequest(format!("{} isn't Resolved", offered.thread))),
        };
        self.human_thread(thread, offered, |t| {
            t.resolved = None;
            t.reopened = true;
        })
    }

    /// Runs `f` on `thread` if its Session offers the action there: `offered` says whether it
    /// does, or why not. Once the Session has ended, nothing is offered.
    fn human_thread(
        &self,
        ThreadRef { session: id, thread }: &ThreadRef,
        offered: impl FnOnce(&ThreadActions) -> Result<(), Error>,
        f: impl FnOnce(&mut Thread),
    ) -> Result<(), Error> {
        let mut inner = self.lock();
        let at = inner.position(id)?;
        let view = &mut inner.sessions[at].view;
        let actions = view.actions();
        let actions = actions.thread(thread).ok_or_else(|| Error::UnknownThread(thread.clone()))?;
        if view.ended.is_some() {
            return Err(Error::Ended);
        }
        offered(actions)?;
        let found = view.threads.iter_mut().find(|t| &t.id == thread).expect("`actions` lists the Session's Threads");
        f(found);
        inner.notify();
        Ok(())
    }

    /// Runs `f` on a Thread a Round brought the agent, in the Session of `source`, with the
    /// number of the Round being drafted.
    fn agent_thread(
        &self,
        source: &SourceRef,
        thread: &ThreadId,
        f: impl FnOnce(u32, &mut Thread),
    ) -> Result<(), Error> {
        let key = self.session_key(source)?;
        let mut inner = self.lock();
        let at = key.find(&inner.sessions)?;
        let session = &mut inner.sessions[at];
        let round = session.view.round;
        // Only a Round tells the agent about a Thread: one of its own Drafts nobody questioned,
        // or one already on the Forge, isn't the agent's to answer.
        let found =
            session.view.threads.iter_mut().find(|t| &t.id == thread && t.comments.iter().any(|c| c.round.is_some()));
        f(round, found.ok_or_else(|| Error::UnknownThread(thread.clone()))?);
        inner.notify();
        Ok(())
    }

    /// What identifies the Session of `source`. Reads the repository, not holding the lock.
    fn session_key(&self, source: &SourceRef) -> Result<SessionKey, Error> {
        Ok(match source {
            SourceRef::Local(cwd) => {
                let repo = git::toplevel(cwd)?;
                let branch = git::branch(&repo)?;
                SessionKey::Local { repo, branch }
            }
            SourceRef::PullRequest(url) => SessionKey::PullRequest(
                self.forges
                    .iter()
                    .find_map(|forge| forge.recognise(url))
                    .ok_or_else(|| Error::NotAPullRequest(url.clone()))?,
                url.clone(),
            ),
        })
    }
}

/// Marks `thread` Resolved by `by`; a reopening not yet handed over no longer needs telling.
fn resolve(thread: &mut Thread, by: Party) {
    thread.resolved = Some(by);
    thread.reopened = false;
}
