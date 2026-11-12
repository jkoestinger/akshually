//! The core review service: the single entry point both the UI and the CLI channel talk to.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::changeset::{self, Changeset, LocalOptions};
use crate::thread::{Anchor, Comment, Party, Round, Thread, ThreadId};
use crate::{Error, Settings, git};

/// Identifies a Session for as long as the app runs.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    pub fn new(id: impl Into<String>) -> Self {
        SessionId(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What the agent's blocking `review` call receives when the human acts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    /// The human handed over a Round of feedback; the Session goes on.
    Round(Round),
    /// The Verdict: the human approved the agent's local work.
    Approved,
    /// The human closed the Session without a Verdict; the agent should stop.
    Abandoned,
}

/// Whose move a Session is waiting on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Turn {
    /// The agent waits on the human: a `review` call is blocked.
    Human,
    /// The human handed over a Round; the agent is working on it.
    Agent,
}

/// An agent asking for review of its local work.
#[derive(Clone, Debug)]
pub struct ReviewRequest {
    /// Any directory inside the repository (or worktree) under Review.
    pub cwd: PathBuf,
    /// Which local changes make up the Changeset.
    pub local: LocalOptions,
}

impl ReviewRequest {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        ReviewRequest { cwd: cwd.into(), local: LocalOptions::default() }
    }

    /// Shows the changes since HEAD's merge-base with `base` instead of the default branch.
    pub fn base(mut self, base: impl Into<String>) -> Self {
        self.local.base = Some(base.into());
        self
    }

    /// Includes untracked, non-ignored files (`true`) or not, whatever the setting says.
    pub fn untracked(mut self, untracked: bool) -> Self {
        self.local.untracked = Some(untracked);
        self
    }

    /// Shows exactly the commit range `<from>..<to>` or `<from>...<to>`.
    pub fn range(mut self, range: impl Into<String>) -> Self {
        self.local.range = Some(range.into());
        self
    }
}

/// What the UI (and tests) can observe of one Session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionView {
    pub id: SessionId,
    /// The working tree root: each worktree is its own Session.
    pub repo: PathBuf,
    pub branch: String,
    /// The full Changeset.
    pub changeset: Changeset,
    /// The Round being drafted: 1 until the first handover, then one more after each.
    pub round: u32,
    /// In the order they were opened.
    pub threads: Vec<Thread>,
    pub turn: Turn,
    /// The Session became the human's Turn while another tab was displayed, and its tab
    /// hasn't been opened since.
    pub attention: bool,
    /// The agent's changes since the Session base, when it ran `start` first.
    pub since_session_base: Option<Changeset>,
    /// Which Changeset the human is looking at.
    pub view: ChangesetView,
}

impl SessionView {
    /// The Changeset the human is looking at.
    pub fn shown(&self) -> &Changeset {
        match (self.view, &self.since_session_base) {
            (ChangesetView::SinceSessionBase, Some(since)) => since,
            _ => &self.changeset,
        }
    }
}

/// Which of a Session's Changesets the human is looking at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangesetView {
    /// Only the agent's changes since the Session base (the default after `start`).
    SinceSessionBase,
    /// The whole Changeset.
    Full,
}

/// A `review` call waiting for the human. Dropping it stops waiting.
pub struct PendingReview {
    session: SessionId,
    outcome: Receiver<Outcome>,
}

impl PendingReview {
    pub fn session(&self) -> &SessionId {
        &self.session
    }

    /// Blocks until the human acts. `None` if the Session stopped waiting for this call
    /// without an outcome.
    pub fn wait(self) -> Option<Outcome> {
        self.outcome.recv().ok()
    }

    /// `wait`, giving up after `timeout`.
    pub fn wait_timeout(&self, timeout: Duration) -> Option<Outcome> {
        self.outcome.recv_timeout(timeout).ok()
    }
}

type Subscriber = Box<dyn Fn(&[SessionView]) + Send>;

struct Session {
    view: SessionView,
    /// The agent's blocked `review` call, if any.
    waiter: Option<Sender<Outcome>>,
    /// Threads opened so far, for numbering the next one.
    threads_opened: u32,
    /// Rounds handed over while no `review` call waited, oldest first.
    queued: Vec<Round>,
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    sessions: Vec<Session>,
    settings: Settings,
    /// Session bases from `start`, by repository and branch, kept until the Session ends.
    session_bases: HashMap<(PathBuf, String), String>,
    subscribers: Vec<Subscriber>,
    /// The Session whose tab the human has open.
    displayed: Option<SessionId>,
}

impl Inner {
    /// The Session at `at` became the human's Turn. It never takes the view: unless it's
    /// displayed it gets attention. With nothing displayed, there's no view to take, so it's
    /// displayed instead.
    fn became_humans_turn(&mut self, at: usize) {
        let view = &mut self.sessions[at].view;
        view.turn = Turn::Human;
        match &self.displayed {
            None => self.displayed = Some(view.id.clone()),
            Some(displayed) => view.attention = displayed != &view.id,
        }
    }

    fn position(&self, id: &SessionId) -> Result<usize, Error> {
        self.sessions.iter().position(|s| &s.view.id == id).ok_or_else(|| Error::UnknownSession(id.clone()))
    }

    fn views(&self) -> Vec<SessionView> {
        self.sessions.iter().map(|s| s.view.clone()).collect()
    }

    fn notify(&self) {
        if self.subscribers.is_empty() {
            return;
        }
        let views = self.views();
        for subscriber in &self.subscribers {
            subscriber(&views);
        }
    }
}

#[derive(Default)]
pub struct Service {
    // One lock for Sessions and subscribers, so subscribers see changes in order.
    inner: Mutex<Inner>,
}

impl Service {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Calls `f` with every Session after each change. `f` runs while the service is locked,
    /// so it must hand the Sessions off (to a channel, a UI thread) rather than call back in.
    pub fn subscribe(&self, f: impl Fn(&[SessionView]) + Send + 'static) {
        self.lock().subscribers.push(Box::new(f));
    }

    pub fn settings(&self) -> Settings {
        self.lock().settings.clone()
    }

    /// Human: changes the settings; they apply from the next agent call.
    pub fn set_settings(&self, settings: Settings) {
        self.lock().settings = settings;
    }

    pub fn sessions(&self) -> Vec<SessionView> {
        self.lock().views()
    }

    pub fn session(&self, id: &SessionId) -> Option<SessionView> {
        let inner = self.lock();
        let at = inner.position(id).ok()?;
        Some(inner.sessions[at].view.clone())
    }

    /// The Session whose tab the human has open, if any.
    pub fn displayed(&self) -> Option<SessionId> {
        self.lock().displayed.clone()
    }

    /// Human: opens the Session's tab, which clears its attention.
    pub fn display(&self, id: &SessionId) -> Result<(), Error> {
        let mut inner = self.lock();
        let at = inner.position(id)?;
        if inner.displayed.as_ref() == Some(id) && !inner.sessions[at].view.attention {
            return Ok(());
        }
        inner.displayed = Some(id.clone());
        inner.sessions[at].view.attention = false;
        inner.notify();
        Ok(())
    }

    /// Agent: snapshots the working tree of the repository containing `cwd` as the Session
    /// base, so the next `review` on this branch shows only the changes made after it.
    pub fn start(&self, cwd: &Path) -> Result<(), Error> {
        let repo = git::toplevel(cwd)?;
        let branch = git::branch(&repo)?;
        let base = changeset::snapshot(&repo)?;
        self.lock().session_bases.insert((repo, branch), base);
        Ok(())
    }

    /// Human: switches the Session between its full Changeset and the changes since the
    /// Session base.
    pub fn show(&self, id: &SessionId, view: ChangesetView) -> Result<(), Error> {
        let mut inner = self.lock();
        let at = inner.position(id)?;
        inner.sessions[at].view.view = view;
        inner.notify();
        Ok(())
    }

    /// Agent: asks for review of the local work in `request.cwd`, opening the Session for that
    /// repository and branch, or attaching to it. Returns once the Session shows the Changeset;
    /// wait on the result for the human's answer.
    pub fn review(&self, request: ReviewRequest) -> Result<PendingReview, Error> {
        let repo = git::toplevel(&request.cwd)?;
        let branch = git::branch(&repo)?;
        let (settings, session_base) = {
            let inner = self.lock();
            (inner.settings.clone(), inner.session_bases.get(&(repo.clone(), branch.clone())).cloned())
        };
        let local = changeset::local(&repo, &request.local, &settings, session_base.as_deref())?;
        let (changeset, since_session_base) = (local.full, local.since_session_base);
        let (tx, rx) = mpsc::channel();

        let mut inner = self.lock();
        let existing = inner.sessions.iter().position(|s| s.view.repo == repo && s.view.branch == branch);
        let id = match existing {
            Some(at) => {
                let session = &mut inner.sessions[at];
                session.view.changeset = changeset;
                session.view.since_session_base = since_session_base;
                if session.queued.is_empty() {
                    session.waiter = Some(tx);
                    if session.view.turn != Turn::Human {
                        inner.became_humans_turn(at);
                    }
                } else {
                    let _ = tx.send(Outcome::Round(session.queued.remove(0)));
                }
                inner.sessions[at].view.id.clone()
            }
            None => {
                inner.next_id += 1;
                let id = SessionId::new(format!("s{}", inner.next_id));
                let view = match since_session_base {
                    Some(_) => ChangesetView::SinceSessionBase,
                    None => ChangesetView::Full,
                };
                let view = SessionView {
                    id: id.clone(),
                    repo,
                    branch,
                    changeset,
                    since_session_base,
                    view,
                    round: 1,
                    threads: Vec::new(),
                    turn: Turn::Human,
                    attention: false,
                };
                inner.sessions.push(Session { view, waiter: Some(tx), threads_opened: 0, queued: Vec::new() });
                let at = inner.sessions.len() - 1;
                inner.became_humans_turn(at);
                id
            }
        };
        inner.notify();
        Ok(PendingReview { session: id, outcome: rx })
    }

    /// Human: opens a Thread at `anchor` with `body` (markdown) as a Draft, which goes out
    /// with the next Round.
    pub fn comment(&self, id: &SessionId, anchor: Anchor, body: impl Into<String>) -> Result<ThreadId, Error> {
        let body = body.into();
        if body.trim().is_empty() {
            return Err(Error::EmptyComment);
        }
        let mut inner = self.lock();
        let at = inner.position(id)?;
        let session = &mut inner.sessions[at];
        let excerpt = anchor.excerpt(session.view.shown()).map_err(Error::InvalidAnchor)?;
        session.threads_opened += 1;
        let thread = ThreadId::new(format!("T{}", session.threads_opened));
        let comment = Comment { author: Party::Human, body, round: None };
        session.view.threads.push(Thread {
            id: thread.clone(),
            anchor,
            excerpt,
            comments: vec![comment],
            resolved: None,
        });
        inner.notify();
        Ok(thread)
    }

    /// Human: hands every Draft over to the agent at once, as the Session's next Round, and
    /// returns its number. The waiting `review` call receives it, or the next one if none waits.
    pub fn hand_over_round(&self, id: &SessionId) -> Result<u32, Error> {
        let mut inner = self.lock();
        let at = inner.position(id)?;
        let session = &mut inner.sessions[at];
        let number = session.view.round;
        let mut threads = Vec::new();
        for thread in &mut session.view.threads {
            let drafts: Vec<Comment> = thread
                .comments
                .iter_mut()
                .filter(|c| c.is_draft())
                .map(|c| {
                    c.round = Some(number);
                    c.clone()
                })
                .collect();
            if !drafts.is_empty() {
                threads.push(Thread { comments: drafts, ..thread.clone() });
            }
        }
        if threads.is_empty() {
            return Err(Error::NoDrafts);
        }
        session.view.round += 1;
        session.view.turn = Turn::Agent;
        let round = Round { number, threads };
        match session.waiter.take() {
            // A waiter that went away gives the Round back, for the next call.
            Some(waiter) => {
                if let Err(mpsc::SendError(Outcome::Round(round))) = waiter.send(Outcome::Round(round)) {
                    session.queued.push(round);
                }
            }
            None => session.queued.push(round),
        }
        inner.notify();
        Ok(number)
    }

    /// Agent: replies to `thread` in the Session for the repository and branch at `cwd`. The
    /// human sees it at once; it counts towards the Round being drafted.
    pub fn reply(&self, cwd: &Path, thread: &ThreadId, body: impl Into<String>) -> Result<(), Error> {
        let body = body.into();
        if body.trim().is_empty() {
            return Err(Error::EmptyComment);
        }
        self.agent_thread(cwd, thread, |round, thread| {
            thread.comments.push(Comment { author: Party::Agent, body, round: Some(round) });
        })
    }

    /// Agent: marks `thread` Resolved in the Session for the repository and branch at `cwd`.
    pub fn resolve(&self, cwd: &Path, thread: &ThreadId) -> Result<(), Error> {
        self.agent_thread(cwd, thread, |_, thread| thread.resolved = Some(Party::Agent))
    }

    /// Human: marks `thread` Resolved, whoever opened it.
    pub fn resolve_thread(&self, id: &SessionId, thread: &ThreadId) -> Result<(), Error> {
        self.human_thread(id, thread, |t| t.resolved = Some(Party::Human))
    }

    /// Human: reopens a Resolved `thread`, whoever resolved it.
    pub fn reopen_thread(&self, id: &SessionId, thread: &ThreadId) -> Result<(), Error> {
        self.human_thread(id, thread, |t| t.resolved = None)
    }

    fn human_thread(&self, id: &SessionId, thread: &ThreadId, f: impl FnOnce(&mut Thread)) -> Result<(), Error> {
        let mut inner = self.lock();
        let at = inner.position(id)?;
        let found = inner.sessions[at].view.threads.iter_mut().find(|t| &t.id == thread);
        f(found.ok_or_else(|| Error::UnknownThread(thread.clone()))?);
        inner.notify();
        Ok(())
    }

    /// Runs `f` on a Thread the agent has received, in the Session for `cwd`, with the number
    /// of the Round being drafted.
    fn agent_thread(&self, cwd: &Path, thread: &ThreadId, f: impl FnOnce(u32, &mut Thread)) -> Result<(), Error> {
        let repo = git::toplevel(cwd)?;
        let branch = git::branch(&repo)?;
        let mut inner = self.lock();
        let Some(session) = inner.sessions.iter_mut().find(|s| s.view.repo == repo && s.view.branch == branch) else {
            return Err(Error::NoSessionHere { repo, branch });
        };
        let round = session.view.round;
        // A Thread still all Drafts hasn't reached the agent, so it can't know its ID.
        let found =
            session.view.threads.iter_mut().find(|t| &t.id == thread && t.comments.iter().any(|c| !c.is_draft()));
        f(round, found.ok_or_else(|| Error::UnknownThread(thread.clone()))?);
        inner.notify();
        Ok(())
    }

    /// Human: approves the Session, which ends it and returns the Verdict to the agent.
    pub fn approve(&self, id: &SessionId) -> Result<(), Error> {
        self.end(id, Outcome::Approved)
    }

    /// Human: closes the Session without a Verdict and tells the agent to stop.
    pub fn abandon(&self, id: &SessionId) -> Result<(), Error> {
        self.end(id, Outcome::Abandoned)
    }

    fn end(&self, id: &SessionId, outcome: Outcome) -> Result<(), Error> {
        let mut inner = self.lock();
        let at = inner.position(id)?;
        let session = inner.sessions.remove(at);
        if inner.displayed.as_ref() == Some(id) {
            inner.displayed = None;
        }
        inner.session_bases.remove(&(session.view.repo.clone(), session.view.branch.clone()));
        if let Some(waiter) = session.waiter {
            // The agent may have gone away; the Session ends regardless.
            let _ = waiter.send(outcome);
        }
        inner.notify();
        Ok(())
    }
}
