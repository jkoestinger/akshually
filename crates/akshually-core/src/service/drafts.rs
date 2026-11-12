//! Draft rules: the human's Drafts, what the next Round carries, handing it over or taking it
//! back, and the co-Reviewer's Drafts the human keeps, edits or discards.

use super::{ChangesetView, Outcome, Service, SessionId, SessionView, ThreadRef, Turn};
use crate::thread::{Anchor, Comment, NewComment, Party, Round, Side, Thread, ThreadId};
use crate::{Changeset, Error};

impl SessionView {
    /// How many Comments and reopened Threads the next Round would carry.
    pub fn to_hand_over(&self) -> usize {
        let co_review = self.is_co_review();
        let comments: usize =
            self.threads.iter().map(|t| (0..t.comments.len()).filter(|&at| goes_out(co_review, t, at)).count()).sum();
        comments + self.threads.iter().filter(|t| t.reopened && !co_review).count()
    }
}

impl Service {
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

    /// Human: keeps the co-Reviewer's Draft `thread`, which becomes the human's own Draft.
    pub fn keep_draft(&self, thread: &ThreadRef) -> Result<(), Error> {
        self.agent_draft(thread, |threads, at| threads[at].comments[0].author = Party::Human)
    }

    /// Human: rewrites the co-Reviewer's Draft `thread` as `comment`, which keeps it: the human
    /// wrote what it says now.
    pub fn edit_draft(&self, thread: &ThreadRef, comment: impl Into<NewComment>) -> Result<(), Error> {
        let comment = comment.into();
        self.agent_draft(thread, |threads, at| {
            check_comment(&threads[at].anchor, &comment)?;
            let NewComment { body, suggestion } = comment;
            threads[at].comments[0] = Comment { author: Party::Human, body, round: None, suggestion };
            Ok(())
        })?
    }

    /// Human: discards the co-Reviewer's Draft `thread`, and with it the Thread.
    pub fn discard_draft(&self, thread: &ThreadRef) -> Result<(), Error> {
        self.agent_draft(thread, |threads, at| {
            threads.remove(at);
        })
    }

    /// Runs `f` on the Session's Threads with the position of `thread`, a Thread the
    /// co-Reviewer opened whose Draft the human hasn't kept, questioned or not.
    fn agent_draft<T>(
        &self,
        ThreadRef { session: id, thread }: &ThreadRef,
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
}

/// Refuses a Comment with no text, or with a suggestion somewhere other than new-side lines.
pub(super) fn check_comment(anchor: &Anchor, comment: &NewComment) -> Result<(), Error> {
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
