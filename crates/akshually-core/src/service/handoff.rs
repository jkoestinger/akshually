//! The blocking-call handoff: an agent's `review` call waits on its Session, receives what the
//! human queued for it (a Round or the outcome), and acknowledges it.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, Weak};
use std::time::Duration;

use super::{Inner, Outcome, Session, SessionId, Turn, lock};

/// A `review` call waiting for the human. Dropping it stops waiting, and frees the Session for
/// the next `review` call.
pub struct PendingReview {
    pub(super) call: Call,
    pub(super) outcome: Receiver<Outcome>,
}

impl PendingReview {
    pub fn session(&self) -> &SessionId {
        &self.call.session
    }

    /// Blocks until the human acts, and acknowledges what it receives. `None` if the Session
    /// stopped waiting for this call without an outcome.
    pub fn wait(self) -> Option<Outcome> {
        let outcome = self.receive();
        if outcome.is_some() {
            self.ack();
        }
        outcome
    }

    /// `wait`, giving up after `timeout`.
    pub fn wait_timeout(&self, timeout: Duration) -> Option<Outcome> {
        let outcome = self.outcome.recv_timeout(timeout).ok();
        if outcome.is_some() {
            self.ack();
        }
        outcome
    }

    /// Blocks until the human acts, without acknowledging what it receives. For a call made
    /// `with_ack`, dropping it before `ack` gives the Round or Verdict to the next call.
    pub fn receive(&self) -> Option<Outcome> {
        self.outcome.recv().ok()
    }

    /// Confirms the agent got what this call received, which is then delivered for good: a
    /// Verdict ends the Session.
    pub fn ack(&self) {
        let waiter = self.call.waiter;
        self.call.with_session(|session| {
            if session.in_flight.as_ref().is_some_and(|(w, _)| *w == waiter) {
                let (_, outcome) = session.in_flight.take().expect("checked");
                session.delivered(&outcome);
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
pub(super) struct Call {
    pub(super) session: SessionId,
    pub(super) waiter: u64,
    pub(super) inner: Weak<Mutex<Inner>>,
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
        let changed = session.view.agent_waiting != waiting || session.finished;
        inner.reap();
        if changed {
            inner.notify();
        } else {
            // Nothing to show, but an ack or a requeue changes what's saved.
            inner.save();
        }
    }
}

impl Drop for PendingReview {
    fn drop(&mut self) {
        self.call.release();
        let waiter = self.call.waiter;
        self.call.with_session(|session| {
            // Received but not acknowledged: the agent may never have seen it. It goes to the
            // call waiting now, if any, or the next one.
            if session.in_flight.as_ref().is_some_and(|(w, _)| *w == waiter) {
                let (_, outcome) = session.in_flight.take().expect("checked");
                session.queued.insert(0, outcome);
                session.deliver();
            }
        });
    }
}

/// The agent's blocked `review` call.
pub(super) struct Waiter {
    pub(super) id: u64,
    pub(super) outcome: Sender<Outcome>,
    /// The call acknowledges what it receives.
    pub(super) ack: bool,
}

impl Session {
    /// Hands the oldest queued Round or Verdict to the waiting `review` call, if both exist.
    pub(super) fn deliver(&mut self) {
        if self.queued.is_empty() {
            return;
        }
        let Some(waiter) = self.waiter.take() else { return };
        let outcome = self.queued.remove(0);
        match waiter.outcome.send(outcome.clone()) {
            Ok(()) => {
                self.view.agent_waiting = false;
                if matches!(outcome, Outcome::Round(_)) {
                    self.view.turn = Turn::Agent;
                }
                if waiter.ack {
                    self.in_flight = Some((waiter.id, outcome));
                } else {
                    self.delivered(&outcome);
                }
            }
            // A waiter that went away without being dropped yet gives it back.
            Err(mpsc::SendError(outcome)) => self.queued.insert(0, outcome),
        }
    }

    /// The agent got `outcome` for good.
    fn delivered(&mut self, outcome: &Outcome) {
        match outcome {
            Outcome::Round(round) => self.replaced_last_rounds.retain(|(n, _)| *n > round.number),
            Outcome::Approved | Outcome::Abandoned | Outcome::Published { .. } | Outcome::Closed => {
                self.finished = true
            }
        }
    }

    /// Ends the Session with `outcome`. With no agent, that's it; otherwise it replaces any
    /// Round the agent hasn't received, and the Session ends once the agent receives it.
    pub(super) fn conclude(&mut self, outcome: Outcome) {
        self.view.ended = Some(outcome.clone());
        if self.view.agent.is_none() {
            return;
        }
        self.queued.clear();
        self.queued.push(outcome);
        self.deliver();
    }
}

impl Inner {
    /// The agent's `review` call `waiter` waits on the Session at `at`: it gets what is queued
    /// for it at once, or else the Session becomes the human's Turn.
    pub(super) fn wait_on(&mut self, at: usize, waiter: Waiter) {
        let session = &mut self.sessions[at];
        session.waiter = Some(waiter);
        session.view.agent_waiting = true;
        if !session.queued.is_empty() {
            session.deliver();
        } else if session.view.turn != Turn::Human {
            self.became_humans_turn(at);
        }
    }
}
