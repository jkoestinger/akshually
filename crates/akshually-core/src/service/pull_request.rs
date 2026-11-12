//! Pull Request Sessions: the Changeset and existing Threads come from a Forge adapter.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use super::{ChangesetView, Service, Session, SessionId, SessionView, Turn};
use crate::forge::{ForgeAdapter, PullRequest, RemoteAnchor, RemoteThread};
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

    /// The adapter serving `pr`.
    fn forge_for(&self, pr: &PullRequest) -> Option<&dyn ForgeAdapter> {
        self.forges.iter().map(|f| f.as_ref()).find(|f| f.recognise(&pr.url).as_ref() == Some(&pr.id))
    }
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
