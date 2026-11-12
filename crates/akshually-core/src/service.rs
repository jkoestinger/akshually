//! The core review service: the single entry point both the UI and the CLI channel talk to.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::changeset::{self, Changeset, LocalOptions};
use crate::forge::{ForgeAdapter, PullRequest, PullRequestRef, PullRequestUpdate};
use crate::store::Store;
use crate::thread::{Anchor, Comment, NewComment, Party, Round, Side, Thread, ThreadId};
use crate::{Error, Notification, Notifier, Settings, git, reanchor};

mod inbox;
mod pull_request;

pub use inbox::{InboxRow, InboxRowState, InboxView};
pub use pull_request::PublishPlan;

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
    /// The human Published their Review of a Pull Request with `verdict`; `url` is the review
    /// on the Forge.
    Published { verdict: Verdict, url: String },
    /// The human closed a co-review of local work, which has no Verdict: a normal end.
    Closed,
}

/// How the human ends a Review of a Pull Request, at Publish. Feedback becomes the Forge's
/// "Comment" only there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Approve,
    RequestChanges,
    Feedback,
}

impl Verdict {
    pub const ALL: [Verdict; 3] = [Verdict::Approve, Verdict::RequestChanges, Verdict::Feedback];
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Verdict::Approve => "approve",
            Verdict::RequestChanges => "request changes",
            Verdict::Feedback => "feedback",
        })
    }
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

/// The part an agent plays in a Session. A Session has the human plus at most one agent, in
/// one role.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The agent wrote the local work under Review; the human gives it Rounds and the Verdict.
    Author,
    /// The agent reviews alongside the human: its Comments are Drafts the human keeps or
    /// discards, and it learns what the human Published.
    CoReviewer,
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Role::Author => "Author",
            Role::CoReviewer => "co-Reviewer",
        })
    }
}

/// An agent asking for review of its local work, as Author, or co-reviewing a Pull Request.
#[derive(Clone, Debug)]
pub struct ReviewRequest {
    /// Any directory inside the repository (or worktree) under Review.
    pub cwd: PathBuf,
    /// Which local changes make up the Changeset.
    pub local: LocalOptions,
    /// Whether the caller acknowledges what it receives (`PendingReview::ack`), as the CLI
    /// channel does. Without, what the call receives counts as delivered at once.
    pub ack: bool,
    /// The URL of a Pull Request the agent co-reviews, instead of local work: the call hands
    /// its Drafts to the human, and returns what the human Published, or Abandoned.
    pub pull_request: Option<String>,
}

impl ReviewRequest {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        ReviewRequest { cwd: cwd.into(), local: LocalOptions::default(), ack: false, pull_request: None }
    }

    /// The co-Reviewer of the Pull Request at `url` hands its Drafts to the human.
    pub fn pull_request(url: impl Into<String>) -> Self {
        ReviewRequest { pull_request: Some(url.into()), ..ReviewRequest::new(PathBuf::new()) }
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

/// Where an agent call finds its Session: the local work in the repository (and branch) at a
/// directory, or a Pull Request by URL. A path converts into the first, a URL into the second.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Local(PathBuf),
    PullRequest(String),
}

impl From<&Path> for Source {
    fn from(cwd: &Path) -> Self {
        Source::Local(cwd.to_owned())
    }
}

impl From<&PathBuf> for Source {
    fn from(cwd: &PathBuf) -> Self {
        Source::Local(cwd.clone())
    }
}

impl From<&str> for Source {
    fn from(url: &str) -> Self {
        Source::PullRequest(url.to_owned())
    }
}

/// What the UI (and tests) can observe of one Session. It's persisted: a field missing from
/// an older saved Session takes its `Default`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionView {
    pub id: SessionId,
    /// The working tree root: each worktree is its own Session. Empty for a Pull Request.
    pub repo: PathBuf,
    /// The branch under Review; a Pull Request's head branch.
    pub branch: String,
    /// The Pull Request under Review, which is the Session's identity; `None` for local work.
    pub pull_request: Option<PullRequest>,
    /// The Session's agent, in its one role; `None` while only the human is in it.
    pub agent: Option<Role>,
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
    /// Files that changed on disk during the human's Turn and differ from the Changeset shown.
    /// They're offered as a reload, never applied: empty means nothing to reload.
    #[serde(default)]
    pub changed_on_disk: BTreeSet<String>,
    /// New commits pushed to the Pull Request since its Changeset was fetched. Offered as a
    /// reload, never applied: `None` means nothing to reload.
    pub pull_request_update: Option<PullRequestUpdate>,
}

impl SessionView {
    /// Whether the agent is a co-Reviewer (a Pull Request takes no other): Rounds then carry
    /// only the human's questions on its Drafts, with the roles reversed.
    pub fn is_co_review(&self) -> bool {
        self.pull_request.is_some() || self.agent == Some(Role::CoReviewer)
    }

    /// How many Comments and reopened Threads the next Round would carry.
    pub fn to_hand_over(&self) -> usize {
        let co_review = self.is_co_review();
        let comments: usize =
            self.threads.iter().map(|t| (0..t.comments.len()).filter(|&at| goes_out(co_review, t, at)).count()).sum();
        comments + self.threads.iter().filter(|t| t.reopened && !co_review).count()
    }

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
    /// What the agent's last `review` call asked for, to rebuild the Changeset from on reload.
    local: LocalOptions,
    /// A Publish is under way. A restart drops it: the call making it is gone.
    #[serde(skip)]
    publishing: bool,
}

impl Session {
    /// Hands the oldest queued Round or Verdict to the waiting `review` call, if both exist.
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
            Outcome::Approved | Outcome::Abandoned | Outcome::Published { .. } | Outcome::Closed => {
                self.finished = true
            }
        }
    }

    /// Opens a Thread at `anchor` whose first Comment is `author`'s Draft `comment` (already
    /// checked with `check_comment`).
    fn open_thread(&mut self, anchor: Anchor, comment: NewComment, author: Party) -> Result<ThreadId, Error> {
        let excerpt = anchor.excerpt(self.view.anchored_in()).map_err(Error::InvalidAnchor)?;
        if !self.view.changeset.takes_comment_at(&anchor) {
            let Anchor::Lines { path, side, start, end } = anchor else { unreachable!("only line ranges are refused") };
            return Err(Error::OutsideDiff { path, side, start, end });
        }
        self.threads_opened += 1;
        let thread = ThreadId::new(format!("T{}", self.threads_opened));
        let NewComment { body, suggestion } = comment;
        self.view.threads.push(Thread {
            id: thread.clone(),
            anchor,
            excerpt,
            comments: vec![Comment { author, body, round: None, suggestion }],
            resolved: None,
            outdated: false,
            reopened: false,
            forge: None,
        });
        Ok(thread)
    }

    /// Ends the Session with `outcome`. With no agent, that's it; otherwise it replaces any
    /// Round the agent hasn't received, and the Session ends once the agent receives it.
    fn conclude(&mut self, outcome: Outcome) {
        self.view.ended = Some(outcome.clone());
        if self.view.agent.is_none() {
            return;
        }
        self.queued.clear();
        self.queued.push(outcome);
        self.deliver();
    }

    /// Lets an agent into the Session as `role`, unless it already has an agent in the other
    /// role.
    fn join(&mut self, role: Role) -> Result<(), Error> {
        match self.view.agent {
            Some(other) if other != role => Err(Error::OtherAgent(other)),
            _ => {
                self.view.agent = Some(role);
                Ok(())
            }
        }
    }

    /// Takes a new full Changeset (and the changes since the Session base): clears viewed ticks
    /// on files that changed, recomputes the changes since the last Round and re-anchors the
    /// Threads. With `default_view`, from Round 2 the view resets to the changes since the last
    /// Round, as on each re-attach.
    fn refresh(&mut self, changeset: Changeset, since_session_base: Option<Changeset>, default_view: bool) {
        let view = &mut self.view;
        let before = view.anchored_in().clone();
        for file in changeset.since(&view.changeset).files {
            view.viewed.remove(&file.path);
        }
        view.changeset = changeset;
        view.since_session_base = since_session_base;
        view.since_last_round = view.last_round.as_ref().map(|s| view.changeset.since(s));
        view.changed_on_disk.clear();
        if default_view && view.since_last_round.is_some() {
            view.view = ChangesetView::SinceLastRound;
        }
        // Threads follow the Changeset they are anchored in.
        if view.anchored_in() != &before {
            let mut threads = std::mem::take(&mut view.threads);
            reanchor(&mut threads, view.anchored_in());
            view.threads = threads;
        }
    }
}

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
            SessionKey::Local { repo, branch } => {
                s.view.pull_request.is_none() && &s.view.repo == repo && &s.view.branch == branch
            }
            SessionKey::PullRequest(pr, _) => s.view.pull_request.as_ref().is_some_and(|p| &p.id == pr),
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
    inbox: inbox::InboxState,
}

impl Inner {
    /// The Session at `at` became the human's Turn. It never takes the view: unless it's
    /// displayed it gets attention. With nothing displayed, there's no view to take, so it's
    /// displayed instead.
    fn became_humans_turn(&mut self, at: usize) {
        let view = &mut self.sessions[at].view;
        view.turn = Turn::Human;
        match &self.displayed {
            // The Inbox is displayed: it keeps the view.
            None if self.inbox.displayed => view.attention = true,
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

    /// The agent's `review` call `waiter` waits on the Session at `at`: it gets what is queued
    /// for it at once, or else the Session becomes the human's Turn.
    fn wait_on(&mut self, at: usize, waiter: Waiter) {
        let session = &mut self.sessions[at];
        session.waiter = Some(waiter);
        session.view.agent_waiting = true;
        if !session.queued.is_empty() {
            session.deliver();
        } else if session.view.turn != Turn::Human {
            self.became_humans_turn(at);
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
    // One lock for Sessions and subscribers, so subscribers see changes in order.
    // One lock for Sessions and subscribers, so subscribers see changes in order. Shared with
    // each PendingReview, which frees its Session when dropped.
    inner: Arc<Mutex<Inner>>,
    /// One per Forge host, asked in order which one serves a Pull Request URL.
    forges: Vec<Box<dyn ForgeAdapter>>,
}

impl Service {
    /// A service that keeps its Sessions in memory only.
    pub fn new() -> Self {
        Self::default()
    }

    /// A service that saves its Sessions under `state_dir` (never inside a repository) and
    /// restores the ones saved there by an earlier run.
    pub fn open(state_dir: impl AsRef<Path>) -> Result<Self, Error> {
        let (mut store, mut sessions, bases) = Store::open(state_dir.as_ref(), |s: &Session| &s.view.id)?;
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
            // Saved before Sessions had roles: local work always has its Author.
            if session.view.pull_request.is_none() && session.view.agent.is_none() {
                session.view.agent = Some(Role::Author);
            }
        }
        let mut inner = Inner { next_id, sessions, session_bases, ..Inner::default() };
        inner.inbox.saved = store.load_inbox();
        inner.store = Some(store);
        Ok(Service { inner: Arc::new(Mutex::new(inner)), forges: Vec::new() })
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
        inner.inbox.displayed = false;
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
        if let Some(url) = &request.pull_request {
            return self.co_review(url, request.ack);
        }
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
                // A co-Reviewer that drafted on this work first co-reviews it.
                session.join(session.view.agent.unwrap_or(Role::Author))?;
                session.local = request.local;
                session.refresh(changeset, since_session_base, true);
                inner.wait_on(at, waiter);
                inner.sessions[at].view.id.clone()
            }
            None => {
                inner.next_id += 1;
                let id = SessionId::new(format!("s{}", inner.next_id));
                let view = SessionView {
                    agent_waiting: true,
                    ..local_view(id.clone(), repo, branch, changeset, since_session_base, Role::Author)
                };
                inner.sessions.push(Session { view, waiter: Some(waiter), local: request.local, ..Session::default() });
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

    /// Agent, as co-Reviewer: opens a Thread in the Session of `source` with `comment`
    /// (markdown, plus any suggestion) as an agent Draft, for the human to keep, edit, discard
    /// or question. Opens the Session if needed, without taking the human's view; a Pull
    /// Request's blocks on the Forge.
    pub fn draft(
        &self,
        source: impl Into<Source>,
        anchor: Anchor,
        comment: impl Into<NewComment>,
    ) -> Result<ThreadId, Error> {
        let comment = comment.into();
        check_comment(&anchor, &comment)?;
        let id = match source.into() {
            Source::PullRequest(url) => self.open_pull_request_session(&url, Some(Role::CoReviewer))?.0,
            Source::Local(cwd) => self.open_local_co_review(&cwd)?,
        };
        let mut inner = self.lock();
        let at = inner.position(&id)?;
        let session = &mut inner.sessions[at];
        if session.view.ended.is_some() {
            return Err(Error::Ended);
        }
        session.join(Role::CoReviewer)?;
        let thread = session.open_thread(anchor, comment, Party::Agent)?;
        inner.notify();
        Ok(thread)
    }

    /// The Session of the local work at `cwd`, opened for a co-Reviewer if needed: it starts in
    /// the agent's Turn, out of the human's way, until the agent's `review` call.
    fn open_local_co_review(&self, cwd: &Path) -> Result<SessionId, Error> {
        let repo = git::toplevel(cwd)?;
        let branch = git::branch(&repo)?;
        let here = |s: &&Session| s.view.pull_request.is_none() && s.view.repo == repo && s.view.branch == branch;
        let (settings, session_base) = {
            let inner = self.lock();
            if let Some(open) = inner.sessions.iter().find(here) {
                return Ok(open.view.id.clone());
            }
            (inner.settings.clone(), inner.session_bases.get(&(repo.clone(), branch.clone())).cloned())
        };
        let options = LocalOptions::default();
        let local = changeset::local(&repo, &options, &settings, session_base.as_deref())?;

        let mut inner = self.lock();
        // Another call may have opened it meanwhile.
        if let Some(open) = inner.sessions.iter().find(here) {
            return Ok(open.view.id.clone());
        }
        inner.next_id += 1;
        let id = SessionId::new(format!("s{}", inner.next_id));
        let view = local_view(
            id.clone(),
            repo.clone(),
            branch.clone(),
            local.full,
            local.since_session_base,
            Role::CoReviewer,
        );
        inner.sessions.push(Session {
            view: SessionView { turn: Turn::Agent, ..view },
            local: options,
            ..Session::default()
        });
        inner.notify();
        Ok(id)
    }

    /// Human: refreshes the Session's Changeset from disk, or for a Pull Request from its Forge
    /// (blocking on it). Threads and Drafts are re-anchored by content, or become Outdated.
    pub fn reload(&self, id: &SessionId) -> Result<(), Error> {
        let (repo, local, settings, session_base) = {
            let inner = self.lock();
            let session = &inner.sessions[inner.position(id)?];
            if let Some(pr) = session.view.pull_request.clone() {
                drop(inner);
                return self.reload_pull_request(id, &pr);
            }
            let key = (session.view.repo.clone(), session.view.branch.clone());
            (key.0.clone(), session.local.clone(), inner.settings.clone(), inner.session_bases.get(&key).cloned())
        };
        let now = changeset::local(&repo, &local, &settings, session_base.as_deref())?;
        let mut inner = self.lock();
        let at = inner.position(id)?;
        inner.sessions[at].refresh(now.full, now.since_session_base, false);
        inner.notify();
        Ok(())
    }

    /// Runs `check_disk` every `every` on a background thread, until the service is dropped.
    pub fn watch_disk(self: &Arc<Self>, every: Duration) {
        let service = Arc::downgrade(self);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(every);
                let Some(service) = service.upgrade() else { return };
                service.check_disk();
            }
        });
    }

    /// Looks for changes on disk in every local Session whose Turn is the human's, and offers
    /// them as a reload (`SessionView::changed_on_disk`) without touching the Changeset. During
    /// the agent's Turn there's nothing to offer: its next `review` refreshes the Changeset.
    pub fn check_disk(&self) {
        let (settings, checks) = {
            let inner = self.lock();
            let checks: Vec<_> = inner
                .sessions
                .iter()
                // A Pull Request has no files on disk.
                .filter(|s| s.view.turn == Turn::Human && s.view.pull_request.is_none())
                .map(|s| {
                    let key = (s.view.repo.clone(), s.view.branch.clone());
                    (s.view.id.clone(), s.local.clone(), inner.session_bases.get(&key).cloned(), key.0)
                })
                .collect();
            (inner.settings.clone(), checks)
        };
        for (id, local, session_base, repo) in checks {
            // A repository that went away or broke has nothing to offer.
            let Ok(now) = changeset::local(&repo, &local, &settings, session_base.as_deref()) else { continue };
            let mut inner = self.lock();
            let Ok(at) = inner.position(&id) else { continue };
            let view = &mut inner.sessions[at].view;
            if view.turn != Turn::Human {
                continue;
            }
            let changed = changed_files(&view.changeset, &now.full);
            if view.changed_on_disk != changed {
                view.changed_on_disk = changed;
                inner.notify();
            }
        }
    }

    /// Human: opens a Thread at `anchor` with `comment` (markdown, plus any suggestion) as a
    /// Draft, which goes out with the next Round.
    pub fn comment(&self, id: &SessionId, anchor: Anchor, comment: impl Into<NewComment>) -> Result<ThreadId, Error> {
        let comment = comment.into();
        check_comment(&anchor, &comment)?;
        let mut inner = self.lock();
        let at = inner.position(id)?;
        let thread = inner.sessions[at].open_thread(anchor, comment, Party::Human)?;
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
        let co_review = session.view.is_co_review();
        let mut threads = Vec::new();
        for thread in &mut session.view.threads {
            let goes_out: Vec<bool> = (0..thread.comments.len()).map(|at| goes_out(co_review, thread, at)).collect();
            let drafts: Vec<Comment> = thread
                .comments
                .iter_mut()
                .zip(goes_out)
                .filter(|(_, goes)| *goes)
                .map(|(c, _)| {
                    c.round = Some(number);
                    c.clone()
                })
                .collect();
            let reopened = thread.reopened && !co_review;
            if !drafts.is_empty() || reopened {
                threads.push(Thread { comments: drafts, reopened, ..thread.clone() });
                thread.reopened = false;
            }
        }
        if threads.is_empty() {
            return Err(Error::NoDrafts);
        }
        session.view.round += 1;
        session.view.turn = Turn::Agent;
        // The agent's next `review` refreshes the Changeset anyway.
        session.view.changed_on_disk.clear();
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

    /// Agent: replies to `thread` in the Session of `source`. The human sees it at once; it
    /// counts towards the Round being drafted.
    pub fn reply(&self, source: impl Into<Source>, thread: &ThreadId, body: impl Into<String>) -> Result<(), Error> {
        let body = body.into();
        if body.trim().is_empty() {
            return Err(Error::EmptyComment);
        }
        self.agent_thread(&source.into(), thread, |round, thread| {
            thread.comments.push(Comment { author: Party::Agent, body, round: Some(round), suggestion: None });
        })
    }

    /// Agent: marks `thread` Resolved in the Session of `source`.
    pub fn resolve(&self, source: impl Into<Source>, thread: &ThreadId) -> Result<(), Error> {
        self.agent_thread(&source.into(), thread, |_, thread| resolve(thread, Party::Agent))
    }

    /// Human: replies in `thread` with `body` (markdown) as a Draft, which goes out with the
    /// next Round under that Thread.
    pub fn reply_in_thread(&self, id: &SessionId, thread: &ThreadId, body: impl Into<String>) -> Result<(), Error> {
        let body = body.into();
        if body.trim().is_empty() {
            return Err(Error::EmptyComment);
        }
        self.human_thread(id, thread, |t| {
            t.comments.push(Comment { author: Party::Human, body, round: None, suggestion: None });
        })
    }

    /// Human: marks `thread` Resolved, whoever opened it.
    pub fn resolve_thread(&self, id: &SessionId, thread: &ThreadId) -> Result<(), Error> {
        self.human_thread(id, thread, |t| resolve(t, Party::Human))
    }

    /// Human: reopens a Resolved `thread`, whoever resolved it. The next Round tells the agent.
    pub fn reopen_thread(&self, id: &SessionId, thread: &ThreadId) -> Result<(), Error> {
        self.human_thread(id, thread, |t| {
            if t.resolved.take().is_some() {
                t.reopened = true;
            }
        })
    }

    /// Human: keeps the co-Reviewer's Draft `thread`, which becomes the human's own Draft.
    pub fn keep_draft(&self, id: &SessionId, thread: &ThreadId) -> Result<(), Error> {
        self.agent_draft(id, thread, |threads, at| threads[at].comments[0].author = Party::Human)
    }

    /// Human: rewrites the co-Reviewer's Draft `thread` as `comment`, which keeps it: the human
    /// wrote what it says now.
    pub fn edit_draft(&self, id: &SessionId, thread: &ThreadId, comment: impl Into<NewComment>) -> Result<(), Error> {
        let comment = comment.into();
        self.agent_draft(id, thread, |threads, at| {
            check_comment(&threads[at].anchor, &comment)?;
            let NewComment { body, suggestion } = comment;
            threads[at].comments[0] = Comment { author: Party::Human, body, round: None, suggestion };
            Ok(())
        })?
    }

    /// Human: discards the co-Reviewer's Draft `thread`, and with it the Thread.
    pub fn discard_draft(&self, id: &SessionId, thread: &ThreadId) -> Result<(), Error> {
        self.agent_draft(id, thread, |threads, at| {
            threads.remove(at);
        })
    }

    /// Runs `f` on the Session's Threads with the position of `thread`, a Thread the
    /// co-Reviewer opened whose Draft the human hasn't kept, questioned or not.
    fn agent_draft<T>(
        &self,
        id: &SessionId,
        thread: &ThreadId,
        f: impl FnOnce(&mut Vec<Thread>, usize) -> T,
    ) -> Result<T, Error> {
        let mut inner = self.lock();
        let at = inner.position(id)?;
        let threads = &mut inner.sessions[at].view.threads;
        let found = threads.iter().position(|t| &t.id == thread).ok_or_else(|| Error::UnknownThread(thread.clone()))?;
        if !threads[found].comments.first().is_some_and(Comment::is_agent_draft) {
            return Err(Error::NotAnAgentDraft(thread.clone()));
        }
        let result = f(threads, found);
        inner.notify();
        Ok(result)
    }

    fn human_thread(&self, id: &SessionId, thread: &ThreadId, f: impl FnOnce(&mut Thread)) -> Result<(), Error> {
        let mut inner = self.lock();
        let at = inner.position(id)?;
        let found = inner.sessions[at].view.threads.iter_mut().find(|t| &t.id == thread);
        f(found.ok_or_else(|| Error::UnknownThread(thread.clone()))?);
        inner.notify();
        Ok(())
    }

    /// Runs `f` on a Thread a Round brought the agent, in the Session of `source`, with the
    /// number of the Round being drafted.
    fn agent_thread(&self, source: &Source, thread: &ThreadId, f: impl FnOnce(u32, &mut Thread)) -> Result<(), Error> {
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
    fn session_key(&self, source: &Source) -> Result<SessionKey, Error> {
        Ok(match source {
            Source::Local(cwd) => {
                let repo = git::toplevel(cwd)?;
                let branch = git::branch(&repo)?;
                SessionKey::Local { repo, branch }
            }
            Source::PullRequest(url) => SessionKey::PullRequest(
                self.forges
                    .iter()
                    .find_map(|forge| forge.recognise(url))
                    .ok_or_else(|| Error::NotAPullRequest(url.clone()))?,
                url.clone(),
            ),
        })
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
    /// A Pull Request Session with no agent has no one to tell, and a co-review of local work
    /// has no Verdict: abandoning either closes it.
    pub fn abandon(&self, id: &SessionId) -> Result<(), Error> {
        let closes = self.lock().sessions.iter().any(|s| {
            &s.view.id == id
                && match s.view.pull_request {
                    Some(_) => s.view.agent.is_none(),
                    None => s.view.agent == Some(Role::CoReviewer),
                }
        });
        if closes {
            return self.close(id);
        }
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
        if session.publishing {
            return Err(Error::Publishing);
        }
        if outcome == Outcome::Approved && session.view.pull_request.is_some() {
            return Err(Error::InvalidRequest("a Pull Request is approved by publishing the Verdict".into()));
        }
        if outcome == Outcome::Approved && session.view.agent == Some(Role::CoReviewer) {
            return Err(Error::InvalidRequest("a co-review of local work has no Verdict: close it".into()));
        }
        if outcome == Outcome::Approved && session.view.turn != Turn::Human {
            return Err(Error::NotYourTurn);
        }
        session.conclude(outcome);
        inner.reap();
        inner.notify();
        Ok(())
    }
}

/// A new Session of the local work in `repo` on `branch`, with `agent` in it, in the human's
/// Turn.
fn local_view(
    id: SessionId,
    repo: PathBuf,
    branch: String,
    changeset: Changeset,
    since_session_base: Option<Changeset>,
    agent: Role,
) -> SessionView {
    let view = match since_session_base {
        Some(_) => ChangesetView::SinceSessionBase,
        None => ChangesetView::Full,
    };
    SessionView {
        id,
        repo,
        branch,
        agent: Some(agent),
        changeset,
        since_session_base,
        view,
        round: 1,
        turn: Turn::Human,
        ..SessionView::default()
    }
}

/// Refuses a Comment with no text, or with a suggestion somewhere other than new-side lines.
fn check_comment(anchor: &Anchor, comment: &NewComment) -> Result<(), Error> {
    if comment.body.trim().is_empty() {
        return Err(Error::EmptyComment);
    }
    if comment.suggestion.is_some() && !matches!(anchor, Anchor::Lines { side: Side::New, .. }) {
        return Err(Error::InvalidSuggestion);
    }
    Ok(())
}

/// Whether the next Round carries `thread`'s Comment at `at`: any Draft for an Author, and for
/// a co-Reviewer only the human's questions, Draft replies in a Thread one of its Drafts opens.
/// The human's other Drafts wait for Publish, and the agent's for the human to keep.
fn goes_out(co_review: bool, thread: &Thread, at: usize) -> bool {
    let comment = &thread.comments[at];
    if !co_review {
        return comment.is_draft();
    }
    at > 0 && comment.is_draft() && comment.author == Party::Human && thread.comments[0].is_agent_draft()
}

/// Marks `thread` Resolved by `by`; a reopening not yet handed over no longer needs telling.
fn resolve(thread: &mut Thread, by: Party) {
    thread.resolved = Some(by);
    thread.reopened = false;
}

/// The paths whose change differs between two Changesets, including files in only one of them.
fn changed_files(then: &Changeset, now: &Changeset) -> BTreeSet<String> {
    let differs = |a: &Changeset, b: &Changeset| {
        a.files.iter().filter(|f| !b.files.contains(f)).map(|f| f.path.clone()).collect::<Vec<_>>()
    };
    differs(then, now).into_iter().chain(differs(now, then)).collect()
}

/// The `n` of a Session id `sn`.
fn id_number(id: &SessionId) -> Option<u64> {
    id.as_str().strip_prefix('s')?.parse().ok()
}
