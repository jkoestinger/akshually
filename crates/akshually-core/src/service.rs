//! The core review service: the single entry point both the UI and the CLI channel talk to.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::changeset::{self, Changeset, LocalOptions};
use crate::forge::{ForgeAdapter, PullRequest, PullRequestUpdate};
use crate::store::Store;
use crate::thread::{Anchor, Comment, NewComment, Party, Round, Side, Thread, ThreadId};
use crate::{Error, Notification, Notifier, Settings, git, reanchor};

mod co_review;
mod disk;
mod drafts;
mod handoff;
mod inbox;
mod pull_request;
mod threads;

pub use handoff::{CancelReview, PendingReview};
pub use inbox::{InboxRow, InboxRowState, InboxView};
pub use pull_request::PublishPlan;

use handoff::{Call, Waiter};

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

/// Names one Thread across Sessions: the Session it's in, and its id there.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ThreadRef {
    pub session: SessionId,
    pub thread: ThreadId,
}

impl ThreadRef {
    pub fn new(session: &SessionId, thread: &ThreadId) -> Self {
        ThreadRef { session: session.clone(), thread: thread.clone() }
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

/// Where an agent call finds its Session's [`Source`]: the local work in the repository (and
/// branch) at a directory, or a Pull Request by URL. A path converts into the first, a URL into
/// the second.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceRef {
    Local(PathBuf),
    PullRequest(String),
}

impl From<&Path> for SourceRef {
    fn from(cwd: &Path) -> Self {
        SourceRef::Local(cwd.to_owned())
    }
}

impl From<&PathBuf> for SourceRef {
    fn from(cwd: &PathBuf) -> Self {
        SourceRef::Local(cwd.clone())
    }
}

impl From<&str> for SourceRef {
    fn from(url: &str) -> Self {
        SourceRef::PullRequest(url.to_owned())
    }
}

/// Where a Session's Changeset comes from, which is the Session's identity: the local work on
/// a branch of a working tree (each worktree is its own), or a Pull Request on a Forge.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Local { repo: PathBuf, branch: String },
    PullRequest(PullRequest),
}

impl Default for Source {
    fn default() -> Self {
        Source::Local { repo: PathBuf::new(), branch: String::new() }
    }
}

/// A `SessionView`'s Source as saved: `"source": {...}`. Sessions saved before it had one gave
/// `repo`, `branch` and `pull_request` (with an empty `repo`) instead, which still load.
mod saved_source {
    use std::path::PathBuf;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::Source;
    use crate::forge::PullRequest;

    pub fn serialize<S: Serializer>(source: &Source, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Saved<'a> {
            source: &'a Source,
        }
        Saved { source }.serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Source, D::Error> {
        #[derive(Default, Deserialize)]
        #[serde(default)]
        struct Saved {
            source: Option<Source>,
            repo: PathBuf,
            branch: String,
            pull_request: Option<PullRequest>,
        }
        Ok(match Saved::deserialize(deserializer)? {
            Saved { source: Some(source), .. } => source,
            Saved { pull_request: Some(pr), .. } => Source::PullRequest(pr),
            Saved { repo, branch, .. } => Source::Local { repo, branch },
        })
    }
}

/// What the UI (and tests) can observe of one Session. It's persisted: a field missing from
/// an older saved Session takes its `Default`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionView {
    pub id: SessionId,
    /// Where the Changeset comes from, which is the Session's identity.
    #[serde(flatten, with = "saved_source")]
    pub source: Source,
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
    /// The Pull Request under Review; `None` for local work.
    pub fn pull_request(&self) -> Option<&PullRequest> {
        match &self.source {
            Source::PullRequest(pr) => Some(pr),
            Source::Local { .. } => None,
        }
    }

    /// The working tree root of the local work under Review; `None` for a Pull Request.
    pub fn repo(&self) -> Option<&Path> {
        match &self.source {
            Source::Local { repo, .. } => Some(repo),
            Source::PullRequest(_) => None,
        }
    }

    /// The branch under Review: for a Pull Request, its head branch.
    pub fn branch(&self) -> &str {
        match &self.source {
            Source::Local { branch, .. } => branch,
            Source::PullRequest(pr) => &pr.head_branch,
        }
    }

    /// Whether this is the Session of the local work in `repo` on `branch`.
    pub(crate) fn is_local(&self, repo: &Path, branch: &str) -> bool {
        matches!(&self.source, Source::Local { repo: r, branch: b } if r == repo && b == branch)
    }

    /// Whether the agent is a co-Reviewer (a Pull Request takes no other): Rounds then carry
    /// only the human's questions on its Drafts, with the roles reversed.
    pub fn is_co_review(&self) -> bool {
        self.pull_request().is_some() || self.agent == Some(Role::CoReviewer)
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

    /// Since the last Round, the old side is that Round's code, not a base: no Thread is on it.
    fn old_side_shown(&self) -> bool {
        self.view != ChangesetView::SinceLastRound
    }

    /// Where each current line Thread sits in the shown Changeset: under its last line. Outdated
    /// Threads, and Threads on lines not shown, have no place.
    pub fn placed_threads(&self) -> Vec<PlacedThread> {
        let shown = self.shown();
        let placed = self.threads.iter().filter(|t| !t.outdated).filter_map(|t| {
            let Anchor::Lines { path, side, end, .. } = &t.anchor else { return None };
            if *side == Side::Old && !self.old_side_shown() {
                return None;
            }
            let file = shown.files.iter().position(|f| &f.path == path)?;
            Some(PlacedThread { file, side: *side, line: *end, thread: t.id.clone() })
        });
        placed.collect()
    }

    /// The Anchor for the lines selected from `start` to `end` (each a side and a 1-based line)
    /// in the shown Changeset's file at `file`. A selection running across sides anchors to its
    /// last line alone. `None` where no Thread can go.
    pub fn anchor_for_selection(&self, file: usize, start: (Side, u32), end: (Side, u32)) -> Option<Anchor> {
        let path = self.shown().files.get(file)?.path.clone();
        let side = end.0;
        if side == Side::Old && !self.old_side_shown() {
            return None;
        }
        let (first, last) = if start.0 == side { (start.1.min(end.1), start.1.max(end.1)) } else { (end.1, end.1) };
        Some(Anchor::lines(side, path, first..=last))
    }

    /// How many Drafts the Session holds: the human's, and the co-Reviewer's not kept yet.
    pub fn drafts(&self) -> usize {
        self.threads.iter().flat_map(|t| &t.comments).filter(|c| c.is_draft()).count()
    }

    /// The Session's name, as its tab shows it: `repo · branch` for local work, `acme/web #482`
    /// for a Pull Request.
    pub fn title(&self) -> String {
        match &self.source {
            Source::Local { repo, branch } => {
                let name = repo.file_name().map_or_else(|| repo.display().to_string(), |n| n.to_string_lossy().into());
                format!("{name} · {branch}")
            }
            Source::PullRequest(pr) => pr.reference(),
        }
    }
}

/// A line Thread's place in the shown Changeset: `file` is its index there, and the Thread
/// sits under `line` on `side`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlacedThread {
    pub file: usize,
    pub side: Side,
    pub line: u32,
    pub thread: ThreadId,
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

type Subscriber = Box<dyn Fn(&[SessionView]) + Send>;

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
            let repo = view.repo().map(|r| r.file_name().unwrap_or(r.as_os_str()).to_string_lossy());
            notifier.notify(Notification {
                session: view.id.clone(),
                title: "Your Turn to review".into(),
                body: format!("{} ({})", repo.unwrap_or_default(), view.branch()),
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
            if let Source::Local { repo, branch } = session.view.source {
                self.session_bases.remove(&(repo, branch));
            }
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
            if session.view.pull_request().is_none() && session.view.agent.is_none() {
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
        let existing = inner.sessions.iter().position(|s| s.view.is_local(&repo, &branch));
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
                && match s.view.source {
                    Source::PullRequest(_) => s.view.agent.is_none(),
                    Source::Local { .. } => s.view.agent == Some(Role::CoReviewer),
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
        if outcome == Outcome::Approved && session.view.pull_request().is_some() {
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
        source: Source::Local { repo, branch },
        agent: Some(agent),
        changeset,
        since_session_base,
        view,
        round: 1,
        turn: Turn::Human,
        ..SessionView::default()
    }
}

/// The `n` of a Session id `sn`.
fn id_number(id: &SessionId) -> Option<u64> {
    id.as_str().strip_prefix('s')?.parse().ok()
}
