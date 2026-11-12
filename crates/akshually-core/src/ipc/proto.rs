//! The wire format: one JSON object per line, one request per connection, each answered by
//! exactly one reply (possibly after a long wait).
//!
//! ```jsonc
//! {"v":1,"op":"review","cwd":"/abs/repo","base":"main"} // client → app; options optional
//! {"v":1,"ok":{"outcome":"approved"}}                // app → client
//! {"v":1,"err":{"code":"version","message":"…"}}
//! ```
//!
//! `review` is the one exception: once the CLI has printed the outcome, it sends
//! `{"v":1,"op":"ack"}` on the same connection and the app replies `{"v":1,"ok":{}}`. A Round
//! or Verdict never acknowledged (the CLI died before printing it) goes to the next `review`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{LocalOptions, SessionId, SessionView, ThreadId};

/// Bumped on any incompatible change, so a newer CLI can tell an older running app apart.
pub const VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Blocks until the human answers; replies with an `Outcome`.
    Review {
        cwd: PathBuf,
        #[serde(flatten)]
        local: LocalOptions,
    },
    /// After a `review` replied: the CLI printed the outcome. Replies `{}`.
    Ack,
    /// Records the Session base; replies `{}` at once.
    Start { cwd: PathBuf },
    /// The agent replies to a Thread in the Session for `cwd`. Replies `{}` at once.
    Reply { cwd: PathBuf, thread: ThreadId, body: String },
    /// The agent resolves a Thread in the Session for `cwd`. Replies `{}` at once.
    Resolve { cwd: PathBuf, thread: ThreadId },
    /// Test control: the app's pid and Sessions, as `AppState`.
    #[serde(rename = "test.state")]
    TestState,
    /// Test control: act as the human through the core review service. Replies `{}`.
    #[serde(rename = "test.human")]
    TestHuman { cmd: HumanCommand },
}

/// The human's side of the review service, for driving a headless app in tests.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanCommand {
    Approve { session: SessionId },
    Abandon { session: SessionId },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppState {
    pub pid: u32,
    pub sessions: Vec<SessionView>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    Ok(serde_json::Value),
    Err(ErrorReply),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct ErrorReply {
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The client and the running app speak different protocol versions.
    Version,
    /// The request couldn't be parsed.
    BadRequest,
    /// The review service refused or failed the request.
    Failed,
    /// Another `review` call already waits on this Session.
    Busy,
    /// A test-control request reached an app not started for tests.
    Forbidden,
}

impl ErrorReply {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        ErrorReply { code, message: message.into() }
    }
}

/// A request or reply with the protocol version alongside.
#[derive(Debug, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub v: u32,
    #[serde(flatten)]
    pub body: T,
}

impl<T> Envelope<T> {
    pub fn new(body: T) -> Self {
        Envelope { v: VERSION, body }
    }
}
