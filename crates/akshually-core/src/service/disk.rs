//! Local work on disk: reloading a Session's Changeset, and watching for changes made during
//! the human's Turn.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use super::{Service, SessionId, Turn};
use crate::{Changeset, Error, changeset};

impl Service {
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
}

/// The paths whose change differs between two Changesets, including files in only one of them.
fn changed_files(then: &Changeset, now: &Changeset) -> BTreeSet<String> {
    let differs = |a: &Changeset, b: &Changeset| {
        a.files.iter().filter(|f| !b.files.contains(f)).map(|f| f.path.clone()).collect::<Vec<_>>()
    };
    differs(then, now).into_iter().chain(differs(now, then)).collect()
}
