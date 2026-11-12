//! akshually's core: all review logic, with no UI dependency (ADR 0001).
//!
//! [`Service`] is the single entry point. The UI calls it in-process; agents reach it through
//! the CLI, which talks to the running app over the local channel in [`ipc`] (ADR 0002).

mod changeset;
pub mod git;
pub mod ipc;
mod paths;
mod service;
mod settings;

pub use changeset::{BinarySizes, Changeset, Collapsed, FileChange, LocalOptions};
pub use paths::Paths;
pub use service::{ChangesetView, Outcome, PendingReview, ReviewRequest, Service, SessionId, SessionView};
pub use settings::{Settings, SettingsError};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Git(#[from] git::GitError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    InvalidRequest(String),
    #[error("no Session {0}")]
    UnknownSession(SessionId),
}
