//! Pull Request Sessions: the Changeset and existing Threads come from a Forge adapter.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use super::{ChangesetView, Outcome, Service, Session, SessionId, SessionView, Turn, Verdict};
use crate::forge::{ForgeAdapter, PullRequest, RemoteAnchor, RemoteThread, Review, ReviewComment};
use crate::thread::{Anchor, Comment, Party, Thread, ThreadId};
use crate::{Changeset, Error};

impl Service {
    /// Serves Pull Requests from `forges`, one adapter per Forge host.
    pub fn with_forges(mut self, forges: Vec<Box<dyn ForgeAdapter>>) -> Self {
        self.forges = forges;
        self
    }

    /// Human: opens the Pull Request at `url` in a Session, with its Changeset and the Threads
    /// already on it, or returns its Session if one is open. Either way the Session is the one
    /// displayed: the human asked for it. Blocks on the Forge.
    pub fn open_pull_request(&self, url: &str) -> Result<SessionId, Error> {
        let (forge, pr) = self
            .forges
            .iter()
            .find_map(|forge| Some((forge, forge.recognise(url)?)))
            .ok_or_else(|| Error::NotAPullRequest(url.to_owned()))?;
        let open = |sessions: &[Session]| {
            let mut sessions = sessions.iter();
            sessions.find(|s| s.view.pull_request.as_ref().is_some_and(|p| p.id == pr)).map(|s| s.view.id.clone())
        };
        // Bound first, so the lock is released before `display` takes it again.
        let already = open(&self.lock().sessions);
        if let Some(id) = already {
            self.display(&id)?;
            return Ok(id);
        }

        // Not holding the lock: the Forge may take a while.
        let pull_request = forge.pull_request(&pr)?;
        let changeset = forge.changeset(&pull_request)?;
        let remote = forge.threads(&pull_request)?;

        let mut inner = self.lock();
        // Another call may have opened it meanwhile.
        if let Some(id) = open(&inner.sessions) {
            drop(inner);
            self.display(&id)?;
            return Ok(id);
        }
        let threads: Vec<Thread> = remote
            .into_iter()
            .map(|t| existing_thread(t, &changeset))
            .enumerate()
            .map(|(i, thread)| Thread { id: ThreadId::new(format!("T{}", i + 1)), ..thread })
            .collect();
        inner.next_id += 1;
        let id = SessionId::new(format!("s{}", inner.next_id));
        let view = SessionView {
            id: id.clone(),
            repo: PathBuf::new(),
            branch: pull_request.head_branch.clone(),
            pull_request: Some(pull_request),
            changeset,
            view: ChangesetView::Full,
            round: 1,
            threads,
            // No agent: the human drafts until Publish.
            turn: Turn::Human,
            ..SessionView::default()
        };
        let threads_opened = view.threads.len() as u32;
        inner.sessions.push(Session { view, threads_opened, ..Session::default() });
        inner.displayed = Some(id.clone());
        inner.notify();
        Ok(id)
    }

    /// Human: Publishes the Session's kept Drafts, `summary` (markdown, may be empty) and
    /// `verdict` to the Forge as one review, and returns its web page. The Session then has
    /// its Verdict, and the human closes it. Blocks on the Forge.
    pub fn publish(&self, id: &SessionId, verdict: Verdict, summary: &str) -> Result<String, Error> {
        let (pr, Drafts { kept, .. }) = {
            let mut inner = self.lock();
            let at = inner.position(id)?;
            let drafts = drafts(&inner.sessions[at].view)?;
            let session = &mut inner.sessions[at];
            if std::mem::replace(&mut session.publishing, true) {
                return Err(Error::Publishing);
            }
            drafts
        };
        // Not holding the lock: the Forge may take a while.
        let sent = self.send_review(&pr, verdict, summary, &kept);

        let mut inner = self.lock();
        // Closing waits for Publish to finish, so the Session is still here.
        let at = inner.position(id)?;
        inner.sessions[at].publishing = false;
        let url = sent?;
        let view = &mut inner.sessions[at].view;
        // Only what was sent: the human may have drafted more meanwhile.
        for sent in &kept {
            let Some(thread) = view.threads.iter_mut().find(|t| t.id == sent.id) else { continue };
            for comment in thread.comments.iter_mut().filter(|c| c.is_draft()).take(sent.comments.len()) {
                comment.round = Some(view.round);
            }
        }
        view.ended = Some(Outcome::Published { verdict, url: url.clone() });
        inner.notify();
        Ok(url)
    }

    /// Sends the review of `kept` Drafts to `pr`'s Forge, if it takes `verdict`.
    fn send_review(&self, pr: &PullRequest, verdict: Verdict, summary: &str, kept: &[Thread]) -> Result<String, Error> {
        let forge = self.forge_for(pr).ok_or_else(|| Error::NotAPullRequest(pr.url.clone()))?;
        if !forge.verdicts(pr)?.contains(&verdict) {
            return Err(Error::VerdictNotOffered(verdict));
        }
        let review = review(verdict, summary, kept);
        if verdict != Verdict::Approve && review.body.is_empty() && review.comments.is_empty() {
            return Err(Error::EmptyReview);
        }
        Ok(forge.publish(pr, &review)?)
    }

    /// Human: closes a Pull Request Session, Published or not; unpublished Drafts go with it.
    /// No agent is waiting on it, so nothing is told.
    pub fn close(&self, id: &SessionId) -> Result<(), Error> {
        let mut inner = self.lock();
        let at = inner.position(id)?;
        let session = &mut inner.sessions[at];
        if session.view.pull_request.is_none() {
            return Err(Error::InvalidRequest("local work ends with a Verdict or by abandoning it".into()));
        }
        if session.publishing {
            return Err(Error::Publishing);
        }
        session.finished = true;
        inner.reap();
        inner.notify();
        Ok(())
    }

    /// Runs `check_pull_requests` every `every` on a background thread, until the service is
    /// dropped.
    pub fn watch_pull_requests(self: &Arc<Self>, every: Duration) {
        let service = Arc::downgrade(self);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(every);
                let Some(service) = service.upgrade() else { return };
                service.check_pull_requests();
            }
        });
    }

    /// Asks the Forge whether each open Pull Request received new pushes, and offers them as a
    /// reload (`SessionView::pull_request_update`) without touching the Changeset. A Forge that
    /// can't be reached offers nothing this time. Blocks on the Forges.
    pub fn check_pull_requests(&self) {
        let open: Vec<(SessionId, PullRequest)> = self
            .lock()
            .sessions
            .iter()
            .filter_map(|s| Some((s.view.id.clone(), s.view.pull_request.clone()?)))
            .collect();
        for (id, pr) in open {
            // Not holding the lock: the Forge may take a while.
            let Some(forge) = self.forge_for(&pr) else { continue };
            let Ok(update) = forge.update(&pr) else { continue };
            let mut inner = self.lock();
            let Ok(at) = inner.position(&id) else { continue };
            let view = &mut inner.sessions[at].view;
            // Reloaded meanwhile: the answer is about a head the Session no longer shows.
            if view.pull_request.as_ref().map(|p| &p.head_sha) != Some(&pr.head_sha) {
                continue;
            }
            if view.pull_request_update != update {
                view.pull_request_update = update;
                inner.notify();
            }
        }
    }

    /// Fetches the Pull Request at its current head and takes its Changeset (see `reload`).
    pub(super) fn reload_pull_request(&self, id: &SessionId, pr: &PullRequest) -> Result<(), Error> {
        let forge = self.forge_for(pr).ok_or_else(|| Error::NotAPullRequest(pr.url.clone()))?;
        // Not holding the lock: the Forge may take a while.
        let now = forge.pull_request(&pr.id)?;
        let changeset = forge.changeset(&now)?;
        let mut inner = self.lock();
        let at = inner.position(id)?;
        let session = &mut inner.sessions[at];
        session.refresh(changeset, None, false);
        session.view.pull_request = Some(now);
        session.view.pull_request_update = None;
        inner.notify();
        Ok(())
    }

    /// Human: what Publish would do now, for the Publish dialog. Blocks on the Forge, which
    /// says which Verdicts it accepts.
    pub fn publish_plan(&self, id: &SessionId) -> Result<PublishPlan, Error> {
        let (pr, Drafts { kept, discarded }) = {
            let inner = self.lock();
            drafts(&inner.sessions[inner.position(id)?].view)?
        };
        let forge = self.forge_for(&pr).ok_or_else(|| Error::NotAPullRequest(pr.url.clone()))?;
        let verdicts = forge.verdicts(&pr)?;
        Ok(PublishPlan { verdicts, kept, discarded })
    }

    /// The adapter serving `pr`.
    fn forge_for(&self, pr: &PullRequest) -> Option<&dyn ForgeAdapter> {
        self.forges.iter().map(|f| f.as_ref()).find(|f| f.recognise(&pr.url).as_ref() == Some(&pr.id))
    }
}

/// A Pull Request Session's Pull Request and Drafts, unless it has ended.
fn drafts(view: &SessionView) -> Result<(PullRequest, Drafts), Error> {
    let Some(pr) = view.pull_request.clone() else {
        return Err(Error::InvalidRequest(format!("Session {} isn't a Pull Request: only one is published", view.id)));
    };
    if view.ended.is_some() {
        return Err(Error::Ended);
    }
    Ok((pr, Drafts::of(view)))
}

/// What Publish would send from a Pull Request Session, and what it wouldn't.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishPlan {
    /// The Verdicts the Forge accepts here, in the order to offer them.
    pub verdicts: Vec<Verdict>,
    /// The Drafts Publish sends, each Thread with only its Drafts.
    pub kept: Vec<Thread>,
    /// The Drafts Publish leaves out, each Thread with only its Drafts.
    pub discarded: Vec<Thread>,
}

/// A Pull Request Session's Drafts, as Publish sorts them: each Thread with only its Drafts.
struct Drafts {
    /// The human's Drafts in new Threads still in the Changeset, which Publish sends.
    kept: Vec<Thread>,
    /// The rest: replies in Threads already on the Forge, and Threads gone Outdated.
    discarded: Vec<Thread>,
}

impl Drafts {
    fn of(view: &SessionView) -> Self {
        let (mut kept, mut discarded) = (Vec::new(), Vec::new());
        for thread in &view.threads {
            let drafts: Vec<Comment> = thread.comments.iter().filter(|c| c.is_draft()).cloned().collect();
            if drafts.is_empty() {
                continue;
            }
            let new = drafts.len() == thread.comments.len();
            let sendable = new && !thread.outdated && drafts.iter().all(|c| c.author == Party::Human);
            let list = if sendable { &mut kept } else { &mut discarded };
            list.push(Thread { comments: drafts, ..thread.clone() });
        }
        Drafts { kept, discarded }
    }
}

/// The review Publish sends: Drafts on the Review as a whole join the summary in its body.
fn review(verdict: Verdict, summary: &str, kept: &[Thread]) -> Review {
    let mut body = vec![summary.trim().to_owned()];
    let mut comments = Vec::new();
    for thread in kept {
        let text = thread.comments.iter().map(|c| c.body.trim()).collect::<Vec<_>>().join("\n\n");
        let suggestion = thread.comments.iter().find_map(|c| c.suggestion.clone());
        let anchor = match &thread.anchor {
            Anchor::Lines { path, side, start, end } => {
                RemoteAnchor::Lines { path: path.clone(), side: *side, start: *start, end: *end }
            }
            Anchor::File { path } => RemoteAnchor::File { path: path.clone() },
            Anchor::Review => {
                body.push(text);
                continue;
            }
        };
        comments.push(ReviewComment { anchor, body: text, suggestion });
    }
    body.retain(|b| !b.is_empty());
    Review { verdict, body: body.join("\n\n"), comments }
}

/// A Thread already on the Forge, as a Session Thread (numbered by the caller). The excerpt is
/// the anchored code, or for a Thread the Forge says is outdated, the diff it kept for it; such
/// a Thread is Outdated here too.
fn existing_thread(remote: RemoteThread, changeset: &Changeset) -> Thread {
    let anchor = match remote.anchor {
        RemoteAnchor::Lines { path, side, start, end } => Anchor::Lines { path, side, start, end },
        RemoteAnchor::File { path } => Anchor::File { path },
    };
    let current = if remote.outdated { None } else { anchor.excerpt(changeset).ok() };
    let outdated = current.is_none();
    let excerpt = current.unwrap_or(remote.diff_hunk);
    let comments = remote
        .comments
        .into_iter()
        // A suggestion stays in the body as the Forge's markdown.
        .map(|c| Comment { author: Party::Other(c.author), body: c.body, round: None, suggestion: None })
        .collect();
    let resolved = remote.resolved_by.map(Party::Other);
    Thread { id: ThreadId::new(""), anchor, excerpt, comments, resolved, outdated, reopened: false }
}
