//! akshually's core: all review logic, with no UI dependency (ADR 0001).
//!
//! [`Service`] is the single entry point. The UI calls it in-process; agents reach it through
//! the CLI, which talks to the running app over the local channel in [`ipc`] (ADR 0002).

mod changeset;
pub mod git;
pub mod ipc;
mod paths;
mod service;

pub use changeset::{Changeset, FileChange};
pub use paths::Paths;
pub use service::{Outcome, PendingReview, ReviewRequest, Service, SessionId, SessionView};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Git(#[from] git::GitError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("no Session {0}")]
    UnknownSession(SessionId),
}
