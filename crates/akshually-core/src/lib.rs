//! akshually's core: all review logic, with no UI dependency (ADR 0001).
//!
//! [`Service`] is the single entry point. The UI calls it in-process; agents reach it through
//! the CLI, which talks to the running app over the local channel in [`ipc`] (ADR 0002).

mod changeset;
pub mod forge;
pub mod git;
pub mod ipc;
mod notifier;
mod paths;
mod reanchor;
mod service;
mod settings;
mod store;
mod thread;

pub use changeset::{BinarySizes, Changeset, Collapsed, FileChange, LocalOptions};
pub use notifier::{Notification, Notifier};
pub use paths::Paths;
pub use reanchor::reanchor;
pub use service::{
    CancelReview, ChangesetView, Outcome, PendingReview, ReviewRequest, Service, SessionId, SessionView, Turn,
};
pub use settings::{Settings, SettingsError};
pub use thread::{Anchor, Comment, NewComment, Party, Round, Side, Thread, ThreadId};

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
    #[error("can't anchor a Thread there: {0}")]
    InvalidAnchor(String),
    #[error("a suggestion replaces lines on the new side; anchor it to some")]
    InvalidSuggestion,
    #[error("a Comment needs some text")]
    EmptyComment,
    #[error("there are no Drafts to hand over")]
    NoDrafts,
    #[error("it's the agent's Turn: take the Turn back first")]
    NotYourTurn,
    #[error("it's already your Turn")]
    AlreadyYourTurn,
    #[error("another `akshually review` is already waiting on this Session")]
    Busy,
    #[error("this Session has ended; the agent's next `akshually review` collects its outcome")]
    Ended,
    #[error("no Session for {} on branch {branch}; run `akshually review` first", repo.display())]
    NoSessionHere { repo: std::path::PathBuf, branch: String },
    #[error("no Thread {0} in this Session")]
    UnknownThread(ThreadId),
    #[error("{0} isn't in the Changeset")]
    NotInChangeset(String),
    #[error("{0} isn't a Pull Request on any configured Forge")]
    NotAPullRequest(String),
    #[error(transparent)]
    Forge(#[from] forge::ForgeError),
}
