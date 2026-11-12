//! Pull Request Sessions: the Changeset and existing Threads come from a Forge adapter.

use std::collections::BTreeSet;
use std::path::PathBuf;

use super::{ChangesetView, Service, Session, SessionId, SessionView, Turn};
use crate::forge::{ForgeAdapter, RemoteAnchor, RemoteThread};
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
        if let Some(id) = open(&self.lock().sessions) {
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
            since_session_base: None,
            last_round: None,
            since_last_round: None,
            view: ChangesetView::Full,
            round: 1,
            threads,
            // No agent: the human drafts until Publish.
            turn: Turn::Human,
            attention: false,
            viewed: BTreeSet::new(),
        };
        let threads_opened = view.threads.len() as u32;
        inner.sessions.push(Session { view, waiter: None, threads_opened, queued: Vec::new() });
        inner.displayed = Some(id.clone());
        inner.notify();
        Ok(id)
    }
}

/// A Thread already on the Forge, as a Session Thread (numbered by the caller). The excerpt is
/// the anchored code, or for an outdated Thread the diff the Forge kept for it.
fn existing_thread(remote: RemoteThread, changeset: &Changeset) -> Thread {
    let anchor = match remote.anchor {
        RemoteAnchor::Lines { path, side, start, end } => Anchor::Lines { path, side, start, end },
        RemoteAnchor::File { path } => Anchor::File { path },
    };
    let current = if remote.outdated { None } else { anchor.excerpt(changeset).ok() };
    let excerpt = current.unwrap_or(remote.diff_hunk);
    let comments = remote
        .comments
        .into_iter()
        // A suggestion stays in the body as the Forge's markdown.
        .map(|c| Comment { author: Party::Other(c.author), body: c.body, round: None, suggestion: None })
        .collect();
    Thread { id: ThreadId::new(""), anchor, excerpt, comments }
}
