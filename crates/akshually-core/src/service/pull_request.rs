//! Pull Request Sessions: the Changeset and existing Threads come from a Forge adapter.

use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use super::handoff::{Call, Waiter};
use super::{ChangesetView, Outcome, PendingReview, Role, Service, Session, SessionId, SessionView, Turn, Verdict};
use crate::forge::{
    ForgeAdapter, PullRequest, PullRequestRef, RemoteAnchor, RemoteThread, Review, ReviewComment, ReviewReply,
};
use crate::thread::{Anchor, Comment, ForgeThread, Party, Thread, ThreadId};
use crate::{Changeset, Error};

impl Service {
    /// Serves Pull Requests from `forges`, one adapter per Forge host.
    pub fn with_forges(mut self, forges: Vec<Box<dyn ForgeAdapter>>) -> Self {
        self.forges = forges;
        self
    }

    /// Human: opens the Pull Request at `url` in a Session, with its Changeset and the Threads
    /// already on it, or returns its Session if one is open. Either way the Session is the one
    /// displayed: the human asked for it, and has now seen it as far as the Inbox goes. Blocks
    /// on the Forge.
    pub fn open_pull_request(&self, url: &str) -> Result<SessionId, Error> {
        let (id, pr) = self.open_pull_request_session(url, None)?;
        let mut inner = self.lock();
        inner.inbox.saved.looked_at(&pr);
        inner.notify_inbox();
        Ok(id)
    }

    /// Agent, as co-Reviewer: hands its Drafts on the Pull Request at `url` to the human, and
    /// waits for what the human Published, or Abandoned (see `review`).
    pub(super) fn co_review(&self, url: &str, ack: bool) -> Result<PendingReview, Error> {
        let (id, _) = self.open_pull_request_session(url, Some(Role::CoReviewer))?;
        let (tx, rx) = mpsc::channel();
        let mut inner = self.lock();
        let at = inner.position(&id)?;
        inner.next_waiter += 1;
        let waiter = Waiter { id: inner.next_waiter, outcome: tx, ack };
        let waiter_id = waiter.id;
        let session = &mut inner.sessions[at];
        if session.waiter.is_some() {
            return Err(Error::Busy);
        }
        // Ended with nothing left to collect: the human Published before any agent joined.
        if session.view.ended.is_some() && session.queued.is_empty() {
            return Err(Error::Ended);
        }
        session.join(Role::CoReviewer)?;
        inner.wait_on(at, waiter);
        // An outcome waiting for this call ends the Session.
        inner.reap();
        inner.notify();
        Ok(PendingReview {
            call: Call { session: id, waiter: waiter_id, inner: Arc::downgrade(&self.inner) },
            outcome: rx,
        })
    }

    /// The Session of the Pull Request at `url`, opened if needed. The human (`agent: None`)
    /// gets it displayed; an agent joins it in its `role`, and a Session it opens starts in
    /// the agent's Turn, out of the human's way.
    pub(super) fn open_pull_request_session(
        &self,
        url: &str,
        agent: Option<Role>,
    ) -> Result<(SessionId, PullRequestRef), Error> {
        let (forge, pr) = self
            .forges
            .iter()
            .find_map(|forge| Some((forge, forge.recognise(url)?)))
            .ok_or_else(|| Error::NotAPullRequest(url.to_owned()))?;
        let open = |sessions: &[Session]| {
            let mut sessions = sessions.iter();
            sessions.find(|s| s.view.pull_request.as_ref().is_some_and(|p| p.id == pr)).map(|s| s.view.id.clone())
        };
        let found = |id: SessionId| -> Result<(SessionId, PullRequestRef), Error> {
            if agent.is_none() {
                self.display(&id)?;
            }
            Ok((id, pr.clone()))
        };
        // Bound first, so the lock is released before `display` takes it again.
        let already = open(&self.lock().sessions);
        if let Some(id) = already {
            return found(id);
        }

        // Not holding the lock: the Forge may take a while.
        let pull_request = forge.pull_request(&pr)?;
        let changeset = forge.changeset(&pull_request)?;
        let remote = forge.threads(&pull_request)?;

        let mut inner = self.lock();
        // Another call may have opened it meanwhile.
        if let Some(id) = open(&inner.sessions) {
            drop(inner);
            return found(id);
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
            agent,
            // The human drafts until Publish; an agent that opens it is still reviewing.
            turn: if agent.is_some() { Turn::Agent } else { Turn::Human },
            ..SessionView::default()
        };
        let threads_opened = view.threads.len() as u32;
        inner.sessions.push(Session { view, threads_opened, ..Session::default() });
        if agent.is_none() {
            inner.displayed = Some(id.clone());
            inner.inbox.displayed = false;
        }
        inner.notify();
        Ok((id, pr))
    }

    /// Human: Publishes the Session's kept Drafts, `summary` (markdown, may be empty) and
    /// `verdict` to the Forge as one review, and returns its web page. The Session then has
    /// its Verdict, and the human closes it. Blocks on the Forge.
    pub fn publish(&self, id: &SessionId, verdict: Verdict, summary: &str) -> Result<String, Error> {
        let (pr, drafts) = {
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
        let sent = self.send_review(&pr, verdict, summary, &drafts);

        let mut inner = self.lock();
        // Closing waits for Publish to finish, so the Session is still here.
        let at = inner.position(id)?;
        inner.sessions[at].publishing = false;
        let url = sent?;
        let session = &mut inner.sessions[at];
        let view = &mut session.view;
        // Only what was sent: the human may have drafted more meanwhile.
        for sent in &drafts.kept {
            let Some(thread) = view.threads.iter_mut().find(|t| t.id == sent.id) else { continue };
            for comment in thread.comments.iter_mut().filter(|c| c.is_draft()).take(sent.comments.len()) {
                comment.round = Some(view.round);
            }
        }
        // The Forge now has them as they were sent, whatever the human did meanwhile.
        let changed = drafts.resolve.iter().map(|t| (t, true)).chain(drafts.reopen.iter().map(|t| (t, false)));
        for (sent, resolved) in changed {
            let thread = view.threads.iter_mut().find(|t| t.id == sent.id);
            if let Some(forge) = thread.and_then(|t| t.forge.as_mut()) {
                forge.resolved = resolved;
            }
        }
        // A co-Reviewer learns what went out; without one, the human closes the Session.
        session.conclude(Outcome::Published { verdict, url: url.clone() });
        inner.reap();
        inner.inbox.saved.published(&pr.id);
        inner.notify();
        inner.notify_inbox();
        Ok(url)
    }

    /// Sends the review of `drafts` to `pr`'s Forge, if it takes `verdict`.
    fn send_review(&self, pr: &PullRequest, verdict: Verdict, summary: &str, drafts: &Drafts) -> Result<String, Error> {
        let forge = self.forge_for(pr).ok_or_else(|| Error::NotAPullRequest(pr.url.clone()))?;
        if !forge.verdicts(pr)?.contains(&verdict) {
            return Err(Error::VerdictNotOffered(verdict));
        }
        let review = review(verdict, summary, drafts);
        if verdict != Verdict::Approve
            && review.body.is_empty()
            && review.comments.is_empty()
            && review.replies.is_empty()
        {
            return Err(Error::EmptyReview);
        }
        Ok(forge.publish(pr, &review)?)
    }

    /// Human: closes a Pull Request Session, Published or not; unpublished Drafts go with it.
    /// A co-Reviewer is told the Session was Abandoned, unless it is still to collect what the
    /// human Published (`Error::Ended`). Closing a co-review of local work is its normal end,
    /// which its co-Reviewer is told as Closed.
    pub fn close(&self, id: &SessionId) -> Result<(), Error> {
        let mut inner = self.lock();
        let at = inner.position(id)?;
        let session = &mut inner.sessions[at];
        if session.view.pull_request.is_none() {
            if session.view.agent != Some(Role::CoReviewer) {
                return Err(Error::InvalidRequest("local work ends with a Verdict or by abandoning it".into()));
            }
            drop(inner);
            return self.end(id, Outcome::Closed);
        }
        if session.publishing {
            return Err(Error::Publishing);
        }
        if session.view.agent.is_some() {
            drop(inner);
            return self.end(id, Outcome::Abandoned);
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

    /// Fetches the Pull Request at its current head and takes its Changeset and the Threads
    /// on it now (see `reload`).
    pub(super) fn reload_pull_request(&self, id: &SessionId, pr: &PullRequest) -> Result<(), Error> {
        let forge = self.forge_for(pr).ok_or_else(|| Error::NotAPullRequest(pr.url.clone()))?;
        // Not holding the lock: the Forge may take a while.
        let now = forge.pull_request(&pr.id)?;
        let changeset = forge.changeset(&now)?;
        let remote = forge.threads(&now)?;
        let mut inner = self.lock();
        let at = inner.position(id)?;
        let session = &mut inner.sessions[at];
        session.refresh(changeset, None, false);
        merge_threads(session, remote);
        session.view.pull_request = Some(now);
        session.view.pull_request_update = None;
        inner.notify();
        Ok(())
    }

    /// Human: what Publish would do now, for the Publish dialog. Blocks on the Forge, which
    /// says which Verdicts it accepts.
    pub fn publish_plan(&self, id: &SessionId) -> Result<PublishPlan, Error> {
        let (pr, Drafts { kept, discarded, resolve, reopen }) = {
            let inner = self.lock();
            drafts(&inner.sessions[inner.position(id)?].view)?
        };
        let forge = self.forge_for(&pr).ok_or_else(|| Error::NotAPullRequest(pr.url.clone()))?;
        let verdicts = forge.verdicts(&pr)?;
        Ok(PublishPlan { verdicts, kept, discarded, resolve, reopen })
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
    /// The Drafts Publish sends, each Thread with only its Drafts: new Threads, and replies in
    /// Threads already on the Forge.
    pub kept: Vec<Thread>,
    /// The Drafts Publish leaves out, each Thread with only its Drafts.
    pub discarded: Vec<Thread>,
    /// Threads already on the Forge that Publish marks Resolved there.
    pub resolve: Vec<Thread>,
    /// Threads Resolved on the Forge that Publish reopens there.
    pub reopen: Vec<Thread>,
}

/// A Pull Request Session's Drafts, as Publish sorts them (each Thread with only its Drafts),
/// and the Threads whose Resolved state it changes on the Forge.
struct Drafts {
    /// The human's Drafts that Publish sends: new Threads still in the Changeset, and replies
    /// in Threads already on the Forge.
    kept: Vec<Thread>,
    /// The rest: new Threads gone Outdated or out of the Forge's diff, and Drafts the human
    /// didn't write.
    discarded: Vec<Thread>,
    /// Threads the human resolved here that are open on the Forge.
    resolve: Vec<Thread>,
    /// Threads the human reopened here that are Resolved on the Forge.
    reopen: Vec<Thread>,
}

impl Drafts {
    fn of(view: &SessionView) -> Self {
        let (mut kept, mut discarded, mut resolve, mut reopen) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for thread in &view.threads {
            match &thread.forge {
                Some(forge) if !forge.resolved && thread.resolved.is_some() => resolve.push(thread.clone()),
                Some(forge) if forge.resolved && thread.resolved.is_none() => reopen.push(thread.clone()),
                _ => {}
            }
            let mut drafts: Vec<Comment> = thread.comments.iter().filter(|c| c.is_draft()).cloned().collect();
            if drafts.is_empty() {
                continue;
            }
            let sendable = match thread.forge {
                // A reply: the Thread is there whatever became of its code.
                Some(_) => drafts.iter().all(|c| c.author == Party::Human),
                // A new Thread sends its opening Draft: what follows it, questions to the
                // co-Reviewer and their answers, stays here. The Forge refuses lines out of its
                // diff, which a reload can leave a Draft on.
                None => {
                    let opening = thread.comments.first().filter(|c| c.is_draft() && c.author == Party::Human);
                    let sendable =
                        opening.is_some() && !thread.outdated && view.changeset.takes_comment_at(&thread.anchor);
                    if sendable {
                        drafts = opening.into_iter().cloned().collect();
                    }
                    sendable
                }
            };
            let list = if sendable { &mut kept } else { &mut discarded };
            list.push(Thread { comments: drafts, ..thread.clone() });
        }
        Drafts { kept, discarded, resolve, reopen }
    }
}

/// The review Publish sends: Drafts on the Review as a whole join the summary in its body, and
/// each existing Thread's Drafts make one reply.
fn review(verdict: Verdict, summary: &str, drafts: &Drafts) -> Review {
    let mut body = vec![summary.trim().to_owned()];
    let (mut comments, mut replies) = (Vec::new(), Vec::new());
    for thread in &drafts.kept {
        let text = thread.comments.iter().map(|c| c.body.trim()).collect::<Vec<_>>().join("\n\n");
        if let Some(forge) = &thread.forge {
            replies.push(ReviewReply { thread: forge.id.clone(), body: text });
            continue;
        }
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
    Review {
        verdict,
        body: body.join("\n\n"),
        comments,
        replies,
        resolve: forge_ids(&drafts.resolve),
        reopen: forge_ids(&drafts.reopen),
    }
}

fn forge_ids(threads: &[Thread]) -> Vec<String> {
    threads.iter().filter_map(|t| Some(t.forge.as_ref()?.id.clone())).collect()
}

/// Takes the Threads on the Forge now into `session`, matched by Forge id: others' Comments and
/// resolutions replace what was fetched before, while the human's Drafts stay, and so does a
/// resolve or reopen of theirs not yet Published. New Threads join with the next numbers. A
/// Thread gone from the Forge goes too, unless it holds Drafts, which stay without a Thread to
/// reply to.
fn merge_threads(session: &mut Session, remote: Vec<RemoteThread>) {
    let mut threads = std::mem::take(&mut session.view.threads);
    threads.retain_mut(|thread| {
        let Some(forge) = &thread.forge else { return true };
        if remote.iter().any(|r| r.id == forge.id) {
            return true;
        }
        thread.forge = None;
        thread.comments.iter().any(Comment::is_draft)
    });
    for remote in remote {
        let now = existing_thread(remote, &session.view.changeset);
        let forge_id = now.forge.as_ref().map(|f| f.id.clone());
        match threads.iter_mut().find(|t| t.forge.as_ref().map(|f| &f.id) == forge_id.as_ref()) {
            Some(thread) => {
                let was_resolved = thread.forge.as_ref().is_some_and(|f| f.resolved);
                let resolved =
                    if thread.resolved.is_some() != was_resolved { thread.resolved.take() } else { now.resolved };
                let mut comments = now.comments;
                comments.extend(thread.comments.drain(..).filter(Comment::is_draft));
                *thread = Thread { id: thread.id.clone(), comments, resolved, reopened: thread.reopened, ..now };
            }
            None => {
                session.threads_opened += 1;
                threads.push(Thread { id: ThreadId::new(format!("T{}", session.threads_opened)), ..now });
            }
        }
    }
    session.view.threads = threads;
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
    let forge = Some(ForgeThread { id: remote.id, resolved: remote.resolved_by.is_some() });
    let resolved = remote.resolved_by.map(Party::Other);
    Thread { id: ThreadId::new(""), anchor, excerpt, comments, resolved, outdated, reopened: false, forge }
}
