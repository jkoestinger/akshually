//! The core review service: the single entry point both the UI and the CLI channel talk to.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::changeset::{self, Changeset, LocalOptions};
use crate::store::Store;
use crate::thread::{Anchor, Comment, NewComment, Party, Round, Side, Thread, ThreadId};
use crate::{Error, Notification, Notifier, Settings, git, reanchor};

/// Identifies a Session, across restarts too.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Turn {
    /// The agent asked for review: the human reads, comments and hands over a Round.
    #[default]
    Human,
    /// The human handed over a Round; the agent is working on it. The human can still read
    /// and draft for the next one, or take the Turn back.
    Agent,
}

/// An agent asking for review of its local work.
#[derive(Clone, Debug)]
pub struct ReviewRequest {
    /// Any directory inside the repository (or worktree) under Review.
    pub cwd: PathBuf,
    /// Which local changes make up the Changeset.
    pub local: LocalOptions,
    /// Whether the caller acknowledges what it receives (`PendingReview::ack`), as the CLI
    /// channel does. Without, what the call receives counts as delivered at once.
    pub ack: bool,
}

impl ReviewRequest {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        ReviewRequest { cwd: cwd.into(), local: LocalOptions::default(), ack: false }
    }

    /// Counts a Round or Verdict as delivered only once the caller acknowledges it: dropping
    /// the call before that gives it to the next `review` call.
    pub fn with_ack(mut self) -> Self {
        self.ack = true;
        self
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

/// What the UI (and tests) can observe of one Session. It's persisted: a field missing from
/// an older saved Session takes its `Default`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionView {
    pub id: SessionId,
    /// The working tree root: each worktree is its own Session.
    pub repo: PathBuf,
    pub branch: String,
    /// The full Changeset.
    pub changeset: Changeset,
    /// The Round being drafted: 1 until the first handover, then one more after each.
    pub round: u32,
    /// Whether an agent's `review` call is connected and waiting for the human. A Round handed
    /// over while none is waits for the next call.
    pub agent_waiting: bool,
    /// In the order they were opened.
    pub threads: Vec<Thread>,
    pub turn: Turn,
    /// The human approved or abandoned the Session while no agent's `review` call received it.
    /// The Session stays until the next `review` call collects this, then ends.
    pub ended: Option<Outcome>,
    /// The Session became the human's Turn while another tab was displayed, and its tab
    /// hasn't been opened since.
    pub attention: bool,
    /// The agent's changes since the Session base, when it ran `start` first.
    pub since_session_base: Option<Changeset>,
    /// The full Changeset as it was when the last Round was handed over; `None` before the
    /// first handover.
    pub last_round: Option<Changeset>,
    /// What changed since `last_round`, with the last Round's code as the old side.
    pub since_last_round: Option<Changeset>,
    /// Which Changeset the human is looking at.
    pub view: ChangesetView,
    /// Paths the human ticked as viewed. A tick clears when its file changes in a new Changeset.
    pub viewed: BTreeSet<String>,
}

impl SessionView {
    /// The Changeset the human is looking at.
    pub fn shown(&self) -> &Changeset {
        match (self.view, &self.since_session_base, &self.since_last_round) {
            (ChangesetView::SinceSessionBase, Some(since), _) => since,
            (ChangesetView::SinceLastRound, _, Some(since)) => since,
            _ => &self.changeset,
        }
    }

    /// The Changeset Threads are anchored in: the shown one, except since the last Round,
    /// where the old side is that Round's code rather than a base and files the agent left
    /// alone are hidden, so Threads stay anchored in the full Changeset.
    pub fn anchored_in(&self) -> &Changeset {
        match self.view {
            ChangesetView::SinceLastRound => &self.changeset,
            _ => self.shown(),
        }
    }
}

/// Which of a Session's Changesets the human is looking at.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangesetView {
    /// Only the agent's changes since the Session base (the default after `start`).
    SinceSessionBase,
    /// Only the changes since the last Round (the default from Round 2 on).
    SinceLastRound,
    /// The whole Changeset.
    #[default]
    Full,
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

type Subscriber = Box<dyn Fn(&[SessionView]) + Send>;

/// The agent's blocked `review` call.
struct Waiter {
    id: u64,
    outcome: Sender<Outcome>,
    /// The call acknowledges what it receives.
    ack: bool,
}

/// A Session as the service keeps it, and as the store saves it: every field but the waiter
/// survives a restart, and a field missing from an older saved Session takes its `Default`.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Session {
    view: SessionView,
    /// The agent's blocked `review` call, if any: at most one per Session. A restart drops
    /// it; the agent's next `review` attaches to the restored Session.
    #[serde(skip)]
    waiter: Option<Waiter>,
    /// Threads opened so far, for numbering the next one.
    threads_opened: u32,
    /// Rounds handed over, then the Verdict (or Abandoned), that no `review` call received
    /// yet, oldest first.
    queued: Vec<Outcome>,
    /// What a `review` call received but hasn't acknowledged yet, and that call's id.
    in_flight: Option<(u64, Outcome)>,
    /// The `last_round` each Round not yet delivered replaced, by Round number, to restore if
    /// the human withdraws it.
    replaced_last_rounds: Vec<(u32, Option<Changeset>)>,
    /// The agent has its Verdict (or Abandoned): the Session is over.
    finished: bool,
}

impl Session {
    /// Hands the oldest queued Round to the waiting `review` call, if both exist.
    fn deliver(&mut self) {
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
            Outcome::Approved | Outcome::Abandoned => self.finished = true,
        }
    }
}

/// A Session base as saved: `session_bases` keys by a tuple, which JSON can't.
#[derive(Serialize, Deserialize)]
struct SessionBase {
    repo: PathBuf,
    branch: String,
    tree: String,
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    next_waiter: u64,
    sessions: Vec<Session>,
    settings: Settings,
    /// Session bases from `start`, by repository and branch, kept until the Session ends.
    session_bases: HashMap<(PathBuf, String), String>,
    subscribers: Vec<Subscriber>,
    /// The Session whose tab the human has open.
    displayed: Option<SessionId>,
    /// Whether the human is looking at akshually's window.
    window_focused: bool,
    notifier: Option<Box<dyn Notifier>>,
    /// Where Sessions are saved; `None` keeps them in memory only.
    store: Option<Store>,
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
        if let Some(notifier) = self.notifier.as_ref().filter(|_| self.settings.notifications && !self.window_focused) {
            let repo = view.repo.file_name().unwrap_or(view.repo.as_os_str()).to_string_lossy();
            notifier.notify(Notification {
                session: view.id.clone(),
                title: "Your Turn to review".into(),
                body: format!("{repo} ({})", view.branch),
            });
        }
    }

    /// Removes the Sessions whose agent got their Verdict (or Abandoned).
    fn reap(&mut self) {
        while let Some(at) = self.sessions.iter().position(|s| s.finished) {
            let session = self.sessions.remove(at);
            if self.displayed.as_ref() == Some(&session.view.id) {
                self.displayed = None;
            }
            self.session_bases.remove(&(session.view.repo, session.view.branch));
        }
    }

    fn position(&self, id: &SessionId) -> Result<usize, Error> {
        self.sessions.iter().position(|s| &s.view.id == id).ok_or_else(|| Error::UnknownSession(id.clone()))
    }

    fn views(&self) -> Vec<SessionView> {
        self.sessions.iter().map(|s| s.view.clone()).collect()
    }

    /// Saves the Sessions and Session bases, if the service has a store.
    fn save(&mut self) {
        let Some(store) = &mut self.store else { return };
        let mut bases: Vec<SessionBase> = self
            .session_bases
            .iter()
            .map(|((repo, branch), tree)| SessionBase {
                repo: repo.clone(),
                branch: branch.clone(),
                tree: tree.clone(),
            })
            .collect();
        bases.sort_by(|a, b| (&a.repo, &a.branch).cmp(&(&b.repo, &b.branch)));
        let saved = store.sync(self.sessions.iter().map(|s| (&s.view.id, s))).and_then(|()| store.sync_bases(&bases));
        // The change already happened in memory; a failed save shouldn't undo it.
        if let Err(error) = saved {
            eprintln!("akshually: couldn't save Sessions: {error}");
        }
    }

    /// Publishes a change: saves it, then tells subscribers.
    fn notify(&mut self) {
        self.save();
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
    /// A service that keeps its Sessions in memory only.
    pub fn new() -> Self {
        Self::default()
    }

    /// A service that saves its Sessions under `state_dir` (never inside a repository) and
    /// restores the ones saved there by an earlier run.
    pub fn open(state_dir: impl AsRef<Path>) -> Result<Self, Error> {
        let (store, mut sessions, bases) = Store::open(state_dir.as_ref(), |s: &Session| &s.view.id)?;
        sessions.sort_by_key(|s| id_number(&s.view.id));
        let next_id = sessions.iter().filter_map(|s| id_number(&s.view.id)).max().unwrap_or(0);
        let bases: Vec<SessionBase> = bases;
        let session_bases = bases.into_iter().map(|b| ((b.repo, b.branch), b.tree)).collect();
        // No `review` call survives a restart: what one received without acknowledging it goes
        // to the next.
        for session in &mut sessions {
            if let Some((_, outcome)) = session.in_flight.take() {
                session.queued.insert(0, outcome);
            }
            session.view.agent_waiting = false;
        }
        let inner = Inner { next_id, sessions, session_bases, store: Some(store), ..Inner::default() };
        Ok(Service { inner: Arc::new(Mutex::new(inner)) })
    }

    /// Shows a desktop notification through `notifier` whenever a Session becomes the human's
    /// Turn while the window is unfocused. Without one, the service notifies no one.
    pub fn with_notifier(self, notifier: impl Notifier + 'static) -> Self {
        self.lock().notifier = Some(Box::new(notifier));
        self
    }

    /// The UI: tells the service whether the human is looking at the window (unfocused until
    /// told otherwise).
    pub fn set_window_focused(&self, focused: bool) {
        self.lock().window_focused = focused;
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        lock(&self.inner)
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
        let mut inner = self.lock();
        inner.session_bases.insert((repo, branch), base);
        inner.save();
        Ok(())
    }

    /// Human: switches the Session between its full Changeset and the changes since the
    /// Session base.
    pub fn show(&self, id: &SessionId, view: ChangesetView) -> Result<(), Error> {
        let mut inner = self.lock();
        let at = inner.position(id)?;
        let session = &mut inner.sessions[at].view;
        if session.view != view {
            let before = session.anchored_in().clone();
            session.view = view;
            // Threads follow the Changeset they are anchored in.
            if session.anchored_in() != &before {
                let mut threads = std::mem::take(&mut session.threads);
                reanchor(&mut threads, session.anchored_in());
                session.threads = threads;
            }
        }
        inner.notify();
        Ok(())
    }

    /// Agent: asks for review of the local work in `request.cwd`, opening the Session for that
    /// repository and branch, or attaching to it. Returns once the Session shows the Changeset;
    /// wait on the result for the human's answer, or for a Round queued for the agent.
    ///
    /// Fails with `Busy` while another `review` call waits on the same Session.
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
        inner.next_waiter += 1;
        let waiter = Waiter { id: inner.next_waiter, outcome: tx, ack: request.ack };
        let waiter_id = waiter.id;
        let existing = inner.sessions.iter().position(|s| s.view.repo == repo && s.view.branch == branch);
        let id = match existing {
            Some(at) => {
                let session = &mut inner.sessions[at];
                if session.waiter.is_some() {
                    return Err(Error::Busy);
                }
                let before = session.view.anchored_in().clone();
                for file in changeset.since(&session.view.changeset).files {
                    session.view.viewed.remove(&file.path);
                }
                session.view.changeset = changeset;
                session.view.since_session_base = since_session_base;
                session.view.since_last_round =
                    session.view.last_round.as_ref().map(|s| session.view.changeset.since(s));
                if session.view.since_last_round.is_some() {
                    session.view.view = ChangesetView::SinceLastRound;
                }
                // Threads follow the Changeset they are anchored in.
                if session.view.anchored_in() != &before {
                    let mut threads = std::mem::take(&mut session.view.threads);
                    reanchor(&mut threads, session.view.anchored_in());
                    session.view.threads = threads;
                }
                session.waiter = Some(waiter);
                session.view.agent_waiting = true;
                if !session.queued.is_empty() {
                    session.deliver();
                } else if session.view.turn != Turn::Human {
                    inner.became_humans_turn(at);
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
                    last_round: None,
                    since_last_round: None,
                    view,
                    round: 1,
                    agent_waiting: true,
                    threads: Vec::new(),
                    turn: Turn::Human,
                    ended: None,
                    attention: false,
                    viewed: BTreeSet::new(),
                };
                inner.sessions.push(Session { view, waiter: Some(waiter), ..Session::default() });
                let at = inner.sessions.len() - 1;
                inner.became_humans_turn(at);
                id
            }
        };
        // A Verdict waiting for this call ends the Session.
        inner.reap();
        inner.notify();
        Ok(PendingReview {
            call: Call { session: id, waiter: waiter_id, inner: Arc::downgrade(&self.inner) },
            outcome: rx,
        })
    }

    /// Human: opens a Thread at `anchor` with `comment` (markdown, plus any suggestion) as a
    /// Draft, which goes out with the next Round.
    pub fn comment(&self, id: &SessionId, anchor: Anchor, comment: impl Into<NewComment>) -> Result<ThreadId, Error> {
        let NewComment { body, suggestion } = comment.into();
        if body.trim().is_empty() {
            return Err(Error::EmptyComment);
        }
        if suggestion.is_some() && !matches!(anchor, Anchor::Lines { side: Side::New, .. }) {
            return Err(Error::InvalidSuggestion);
        }
        let mut inner = self.lock();
        let at = inner.position(id)?;
        let session = &mut inner.sessions[at];
        let excerpt = anchor.excerpt(session.view.anchored_in()).map_err(Error::InvalidAnchor)?;
        session.threads_opened += 1;
        let thread = ThreadId::new(format!("T{}", session.threads_opened));
        let comment = Comment { author: Party::Human, body, round: None, suggestion };
        session.view.threads.push(Thread {
            id: thread.clone(),
            anchor,
            excerpt,
            comments: vec![comment],
            outdated: false,
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
        if session.view.ended.is_some() {
            return Err(Error::Ended);
        }
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
        session.view.since_last_round =
            Some(Changeset { base: session.view.changeset.base.clone(), files: Vec::new() });
        let replaced = session.view.last_round.replace(session.view.changeset.clone());
        session.replaced_last_rounds.push((number, replaced));
        session.queued.push(Outcome::Round(Round { number, threads }));
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
        if session.view.ended.is_some() {
            return Err(Error::Ended);
        }
        if session.view.turn == Turn::Human {
            return Err(Error::AlreadyYourTurn);
        }
        // Rounds no agent received yet go back to being Drafts, for the human to keep editing,
        // and the last Round is the one before them again.
        let first = session.queued.iter().find_map(|o| match o {
            Outcome::Round(round) => Some(round.number),
            _ => None,
        });
        if let Some(first) = first {
            session.queued.clear();
            for comment in session.view.threads.iter_mut().flat_map(|t| &mut t.comments) {
                if comment.author == Party::Human && comment.round.is_some_and(|n| n >= first) {
                    comment.round = None;
                }
            }
            session.view.round = first;
            if let Some(at) = session.replaced_last_rounds.iter().position(|(n, _)| *n == first) {
                // The withdrawn Rounds' entries go too.
                let (_, last_round) = session.replaced_last_rounds.drain(at..).next().expect("found at `at`");
                session.view.since_last_round = last_round.as_ref().map(|s| session.view.changeset.since(s));
                session.view.last_round = last_round;
                if session.view.since_last_round.is_none() && session.view.view == ChangesetView::SinceLastRound {
                    session.view.view = ChangesetView::Full;
                }
            }
        }
        inner.became_humans_turn(at);
        inner.notify();
        Ok(())
    }

    /// Human: ticks `path` as viewed, or unticks it.
    pub fn mark_viewed(&self, id: &SessionId, path: &str, viewed: bool) -> Result<(), Error> {
        let mut inner = self.lock();
        let at = inner.position(id)?;
        let view = &mut inner.sessions[at].view;
        if !view.changeset.files.iter().any(|f| f.path == path) {
            return Err(Error::NotInChangeset(path.to_owned()));
        }
        if viewed {
            view.viewed.insert(path.to_owned());
        } else {
            view.viewed.remove(path);
        }
        inner.notify();
        Ok(())
    }

    /// Human: approves the Session, which ends it and returns the Verdict to the agent. Only
    /// on the human's Turn: during the agent's, take the Turn back first.
    pub fn approve(&self, id: &SessionId) -> Result<(), Error> {
        self.end(id, Outcome::Approved)
    }

    /// Human: closes the Session without a Verdict and tells the agent to stop.
    pub fn abandon(&self, id: &SessionId) -> Result<(), Error> {
        self.end(id, Outcome::Abandoned)
    }

    /// Gives the agent `outcome`, which ends the Session once delivered. With no `review` call
    /// waiting, the Session keeps it for the next one.
    fn end(&self, id: &SessionId, outcome: Outcome) -> Result<(), Error> {
        let mut inner = self.lock();
        let at = inner.position(id)?;
        let session = &mut inner.sessions[at];
        if session.view.ended.is_some() {
            return Err(Error::Ended);
        }
        if outcome == Outcome::Approved && session.view.turn != Turn::Human {
            return Err(Error::NotYourTurn);
        }
        session.view.ended = Some(outcome.clone());
        // It replaces any Round the agent hasn't received.
        session.queued.clear();
        session.queued.push(outcome);
        session.deliver();
        inner.reap();
        inner.notify();
        Ok(())
    }
}

/// The `n` of a Session id `sn`.
fn id_number(id: &SessionId) -> Option<u64> {
    id.as_str().strip_prefix('s')?.parse().ok()
}
