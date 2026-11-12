//! A co-Reviewer's Drafts, on a Pull Request or on local work: `draft` opens the Session if
//! needed, out of the human's way.

use std::path::Path;

use super::drafts::check_comment;
use super::{Role, Service, Session, SessionId, SessionView, Source, Turn, local_view};
use crate::thread::{Anchor, NewComment, Party, ThreadId};
use crate::{Error, LocalOptions, changeset, git};

impl Service {
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
}
