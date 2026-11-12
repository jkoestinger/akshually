//! How akshually reaches the human when they aren't looking: OS desktop notifications.

use crate::SessionId;

/// One desktop notification: a Session became the human's Turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    /// The Session to open when the human clicks it.
    pub session: SessionId,
    pub title: String,
    pub body: String,
}

/// Shows desktop notifications. The service calls it while it is locked, so `notify` must
/// return quickly (hand slow work to another thread) and never call back into the service.
pub trait Notifier: Send + Sync {
    fn notify(&self, notification: Notification);
}
