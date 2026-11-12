//! How akshually reaches the human when they aren't looking: OS desktop notifications.

use crate::SessionId;

/// One desktop notification: a Session became the human's Turn, or something went wrong with
/// the app itself, such as agents no longer reaching it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    /// The Session to open when the human clicks it; `None` when it is about the app itself.
    pub session: Option<SessionId>,
    pub title: String,
    pub body: String,
}

/// Shows desktop notifications. The service calls it while it is locked, so `notify` must
/// return quickly (hand slow work to another thread) and never call back into the service.
pub trait Notifier: Send + Sync {
    fn notify(&self, notification: Notification);
}
