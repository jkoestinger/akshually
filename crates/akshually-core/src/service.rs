//! The core review service: the single entry point both the UI and the CLI channel talk to.

use std::fmt;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::changeset::{self, Changeset};
use crate::thread::{Anchor, Comment, Party, Round, Thread, ThreadId};
use crate::{Error, git};

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
    /// The agent asked for review: the human reads, comments and hands over a Round.
    Human,
    /// The agent is addressing a Round. The human can still read and draft for the next one.
    Agent,
}

/// An agent asking for review of its local work.
#[derive(Clone, Debug)]
pub struct ReviewRequest {
    /// Any directory inside the repository (or worktree) under Review.
    pub cwd: PathBuf,
}

impl ReviewRequest {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        ReviewRequest { cwd: cwd.into() }
    }
}

/// What the UI (and tests) can observe of one Session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionView {
    pub id: SessionId,
    /// The working tree root: each worktree is its own Session.
    pub repo: PathBuf,
    pub branch: String,
    pub changeset: Changeset,
    /// The Round being drafted: 1 until the first handover, then one more after each.
    pub round: u32,
    pub turn: Turn,
    /// Whether an agent's `review` call is connected and waiting for the human. A Round handed
    /// over while none is waits for the next call.
    pub agent_waiting: bool,
    /// In the order they were opened.
    pub threads: Vec<Thread>,
}

/// A `review` call waiting for the human. Dropping it stops waiting, and frees the Session for
/// the next `review` call.
pub struct PendingReview {
    call: Call,
    outcome: Receiver<Outcome>,
}

impl PendingReview {
    pub fn session(&self) -> &SessionId {
        &self.call.session
    }

    /// Blocks until the human acts, and acknowledges what it receives. `None` if the Session
    /// stopped waiting for this call without an outcome.
    pub fn wait(self) -> Option<Outcome> {
        let outcome = self.receive();
        self.ack();
        outcome
    }

    /// `wait`, giving up after `timeout`.
    pub fn wait_timeout(&self, timeout: Duration) -> Option<Outcome> {
        let outcome = self.outcome.recv_timeout(timeout).ok();
        self.ack();
        outcome
    }

    /// Blocks until the human acts, without acknowledging a Round: until `ack`, dropping this
    /// call queues the Round again for the next one.
    pub fn receive(&self) -> Option<Outcome> {
        self.outcome.recv().ok()
    }

    /// Confirms the agent got the Round received, which is then delivered for good.
    pub fn ack(&self) {
        let waiter = self.call.waiter;
        self.call.with_session(|session| {
            if session.in_flight.as_ref().is_some_and(|(w, _)| *w == waiter) {
                session.in_flight = None;
            }
        });
    }

    /// A handle that stops this call waiting from another thread, e.g. when the CLI process
    /// behind it went away.
    pub fn canceller(&self) -> CancelReview {
        CancelReview(self.call.clone())
    }
}

/// Stops a `review` call waiting: its `receive` returns `None`, and the Session takes the next
/// `review` call.
pub struct CancelReview(Call);

impl CancelReview {
    pub fn cancel(&self) {
        self.0.release();
    }
}

/// One `review` call on one Session, as seen from outside the service lock.
#[derive(Clone)]
struct Call {
    session: SessionId,
    waiter: u64,
    inner: Weak<Mutex<Inner>>,
}

impl Call {
    /// Frees the Session's waiter slot if this call holds it.
    fn release(&self) {
        let waiter = self.waiter;
        self.with_session(|session| {
            if session.waiter.as_ref().is_some_and(|w| w.id == waiter) {
                session.waiter = None;
            }
        });
    }

    fn with_session(&self, f: impl FnOnce(&mut Session)) {
        let Some(inner) = self.inner.upgrade() else { return };
        let mut inner = lock(&inner);
        let Ok(at) = inner.position(&self.session) else { return };
        let session = &mut inner.sessions[at];
        let waiting = session.view.agent_waiting;
        f(session);
        session.view.agent_waiting = session.waiter.is_some();
        if session.view.agent_waiting != waiting {
            inner.notify();
        }
    }
}

impl Drop for PendingReview {
    fn drop(&mut self) {
        self.call.release();
        let waiter = self.call.waiter;
        self.call.with_session(|session| {
            // Received but not acknowledged: the agent may never have seen it.
            if session.in_flight.as_ref().is_some_and(|(w, _)| *w == waiter) {
                let (_, round) = session.in_flight.take().expect("checked");
                session.queued.insert(0, round);
            }
        });
    }
}

type Subscriber = Box<dyn Fn(&[SessionView]) + Send>;

/// The agent's blocked `review` call.
struct Waiter {
    id: u64,
    outcome: Sender<Outcome>,
}

struct Session {
    view: SessionView,
    /// The agent's blocked `review` call, if any: at most one per Session.
    waiter: Option<Waiter>,
    /// Threads opened so far, for numbering the next one.
    threads_opened: u32,
    /// Rounds handed over that no `review` call received yet, oldest first.
    queued: Vec<Round>,
    /// The Round a `review` call received but hasn't acknowledged yet, and that call's id.
    in_flight: Option<(u64, Round)>,
}

impl Session {
    /// Hands the oldest queued Round to the waiting `review` call, if both exist.
    fn deliver(&mut self) {
        if self.queued.is_empty() {
            return;
        }
        let Some(waiter) = self.waiter.take() else { return };
        let round = self.queued.remove(0);
        match waiter.outcome.send(Outcome::Round(round.clone())) {
            Ok(()) => {
                self.in_flight = Some((waiter.id, round));
                self.view.turn = Turn::Agent;
                self.view.agent_waiting = false;
            }
            // A waiter that went away without being dropped yet gives the Round back.
            Err(_) => self.queued.insert(0, round),
        }
    }
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    next_waiter: u64,
    sessions: Vec<Session>,
    subscribers: Vec<Subscriber>,
}

impl Inner {
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

fn lock(inner: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Default)]
pub struct Service {
    // One lock for Sessions and subscribers, so subscribers see changes in order. Shared with
    // each PendingReview, which frees its Session when dropped.
    inner: Arc<Mutex<Inner>>,
}

impl Service {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        lock(&self.inner)
    }

    /// Calls `f` with every Session after each change. `f` runs while the service is locked,
    /// so it must hand the Sessions off (to a channel, a UI thread) rather than call back in.
    pub fn subscribe(&self, f: impl Fn(&[SessionView]) + Send + 'static) {
        self.lock().subscribers.push(Box::new(f));
    }

    pub fn sessions(&self) -> Vec<SessionView> {
        self.lock().views()
    }

    pub fn session(&self, id: &SessionId) -> Option<SessionView> {
        let inner = self.lock();
        let at = inner.position(id).ok()?;
        Some(inner.sessions[at].view.clone())
    }

    /// Agent: asks for review of the local work in `request.cwd`, opening the Session for that
    /// repository and branch, or attaching to it. Returns once the Session shows the Changeset;
    /// wait on the result for the human's answer, or for a Round queued for the agent.
    ///
    /// Fails with `Busy` while another `review` call waits on the same Session.
    pub fn review(&self, request: ReviewRequest) -> Result<PendingReview, Error> {
        let repo = git::toplevel(&request.cwd)?;
        let branch = git::branch(&repo)?;
        let changeset = changeset::local(&repo)?;
        let (tx, rx) = mpsc::channel();

        let mut inner = self.lock();
        inner.next_waiter += 1;
        let waiter = Waiter { id: inner.next_waiter, outcome: tx };
        let waiter_id = waiter.id;
        let existing = inner.sessions.iter().position(|s| s.view.repo == repo && s.view.branch == branch);
        let id = match existing {
            Some(at) => {
                let session = &mut inner.sessions[at];
                if session.waiter.is_some() || session.in_flight.is_some() {
                    return Err(Error::Busy);
                }
                session.view.changeset = changeset;
                session.waiter = Some(waiter);
                session.view.agent_waiting = true;
                if session.queued.is_empty() {
                    session.view.turn = Turn::Human;
                } else {
                    session.deliver();
                }
                session.view.id.clone()
            }
            None => {
                inner.next_id += 1;
                let id = SessionId::new(format!("s{}", inner.next_id));
                let view = SessionView {
                    id: id.clone(),
                    repo,
                    branch,
                    changeset,
                    round: 1,
                    turn: Turn::Human,
                    agent_waiting: true,
                    threads: Vec::new(),
                };
                inner.sessions.push(Session {
                    view,
                    waiter: Some(waiter),
                    threads_opened: 0,
                    queued: Vec::new(),
                    in_flight: None,
                });
                id
            }
        };
        inner.notify();
        Ok(PendingReview {
            call: Call { session: id, waiter: waiter_id, inner: Arc::downgrade(&self.inner) },
            outcome: rx,
        })
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
        let excerpt = anchor.excerpt(&session.view.changeset).map_err(Error::InvalidAnchor)?;
        session.threads_opened += 1;
        let thread = ThreadId::new(format!("T{}", session.threads_opened));
        let comment = Comment { author: Party::Human, body, round: None };
        session.view.threads.push(Thread { id: thread.clone(), anchor, excerpt, comments: vec![comment] });
        inner.notify();
        Ok(thread)
    }

    /// Human: hands every Draft over to the agent at once, as the Session's next Round, and
    /// returns its number. The waiting `review` call receives it, or the next one if none waits.
    pub fn hand_over_round(&self, id: &SessionId) -> Result<u32, Error> {
        let mut inner = self.lock();
        let at = inner.position(id)?;
        let session = &mut inner.sessions[at];
        if session.view.turn != Turn::Human {
            return Err(Error::NotYourTurn);
        }
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
        session.queued.push(round);
        session.deliver();
        inner.notify();
        Ok(number)
    }

    /// Human: takes the Turn back from an agent that went quiet, to go on drafting, hand over
    /// another Round, or abandon the Session.
    pub fn take_turn_back(&self, id: &SessionId) -> Result<(), Error> {
        let mut inner = self.lock();
        let at = inner.position(id)?;
        let session = &mut inner.sessions[at];
        if session.view.turn == Turn::Human {
            return Err(Error::AlreadyYourTurn);
        }
        session.view.turn = Turn::Human;
        // Rounds no agent received yet go back to being Drafts, for the human to keep editing.
        if let Some(first) = session.queued.first().map(|r| r.number) {
            session.queued.clear();
            for comment in session.view.threads.iter_mut().flat_map(|t| &mut t.comments) {
                if comment.author == Party::Human && comment.round.is_some_and(|n| n >= first) {
                    comment.round = None;
                }
            }
            session.view.round = first;
        }
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
        if let Some(waiter) = session.waiter {
            // The agent may have gone away; the Session ends regardless.
            let _ = waiter.outcome.send(outcome);
        }
        inner.notify();
        Ok(())
    }
}
