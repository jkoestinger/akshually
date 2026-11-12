//! The CLI's end of the local channel: one connection to the running app per request.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use serde::de::{DeserializeOwned, IgnoredAny};

use super::proto::{AppState, Envelope, ErrorCode, ErrorReply, HumanCommand, Reply, Request, VERSION};
use crate::{Anchor, LocalOptions, NewComment, Outcome, Paths, ThreadId};

/// Why a request to the running app failed.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("lost the connection to akshually: {0}")]
    Io(#[from] io::Error),
    #[error("akshually sent a reply this CLI doesn't understand: {0}")]
    Protocol(String),
    #[error(transparent)]
    App(#[from] ErrorReply),
}

/// Connects to the running app. `NotFound` or `ConnectionRefused` mean no app is running.
pub fn connect(paths: &Paths) -> io::Result<Connection> {
    Ok(Connection { stream: UnixStream::connect(paths.socket())? })
}

/// One connection to the running app, used for exactly one request.
pub struct Connection {
    stream: UnixStream,
}

impl Connection {
    /// Asks for review of the local work in `cwd` and blocks until the human answers, then
    /// hands the outcome to `deliver` (which prints it). It is acknowledged only once `deliver`
    /// succeeds; until then the app keeps it for the next `review` call.
    pub fn review(
        self,
        cwd: &Path,
        local: LocalOptions,
        deliver: impl FnOnce(&Outcome) -> io::Result<()>,
    ) -> Result<Outcome, ClientError> {
        self.wait(Request::Review { cwd: cwd.to_owned(), local, pr: None }, deliver)
    }

    /// As co-Reviewer of the Pull Request at `url`, hands the agent's Drafts to the human and
    /// blocks until the human Publishes or abandons, like `review`.
    pub fn co_review(
        self,
        url: &str,
        deliver: impl FnOnce(&Outcome) -> io::Result<()>,
    ) -> Result<Outcome, ClientError> {
        let request = Request::Review { cwd: Default::default(), local: LocalOptions::default(), pr: Some(url.into()) };
        self.wait(request, deliver)
    }

    /// Adds a co-Reviewer Draft at `anchor` on the Pull Request at `pr`, or without one on the
    /// local work in `cwd`, and returns its Thread.
    pub fn draft(
        self,
        cwd: &Path,
        pr: Option<&str>,
        anchor: Anchor,
        comment: NewComment,
    ) -> Result<ThreadId, ClientError> {
        #[derive(serde::Deserialize)]
        struct Drafted {
            thread: ThreadId,
        }
        let NewComment { body, suggestion } = comment;
        let request = Request::Draft { cwd: cwd.to_owned(), pr: pr.map(Into::into), anchor, body, suggestion };
        self.call::<Drafted>(request).map(|d| d.thread)
    }

    /// Sends a blocking `review` request, hands its outcome to `deliver`, then acknowledges it.
    fn wait(
        mut self,
        request: Request,
        deliver: impl FnOnce(&Outcome) -> io::Result<()>,
    ) -> Result<Outcome, ClientError> {
        self.send(request)?;
        let mut reader = BufReader::new(self.stream.try_clone()?);
        let outcome: Outcome = read_reply(&mut reader)?;
        deliver(&outcome)?;
        self.send(Request::Ack)?;
        read_reply::<IgnoredAny>(&mut reader)?;
        Ok(outcome)
    }

    /// Records the Session base for the local work in `cwd`.
    pub fn start(self, cwd: &Path) -> Result<(), ClientError> {
        self.call::<IgnoredAny>(Request::Start { cwd: cwd.to_owned() }).map(drop)
    }

    /// Replies to `thread` in the Session of the Pull Request `pr`, or without one of `cwd`,
    /// as the agent.
    pub fn reply(self, cwd: &Path, pr: Option<&str>, thread: &ThreadId, body: &str) -> Result<(), ClientError> {
        let (cwd, pr, thread, body) = (cwd.to_owned(), pr.map(Into::into), thread.clone(), body.to_owned());
        self.call::<IgnoredAny>(Request::Reply { cwd, pr, thread, body }).map(drop)
    }

    /// Resolves `thread` in the Session of the Pull Request `pr`, or without one of `cwd`, as
    /// the agent.
    pub fn resolve(self, cwd: &Path, pr: Option<&str>, thread: &ThreadId) -> Result<(), ClientError> {
        let (cwd, pr, thread) = (cwd.to_owned(), pr.map(Into::into), thread.clone());
        self.call::<IgnoredAny>(Request::Resolve { cwd, pr, thread }).map(drop)
    }

    /// Brings the running app's window up, activated with `activation_token` if given.
    pub fn show(self, activation_token: Option<String>) -> Result<(), ClientError> {
        self.call::<IgnoredAny>(Request::Show { activation_token }).map(drop)
    }

    /// Test control: the app's pid and Sessions.
    pub fn test_state(self) -> Result<AppState, ClientError> {
        self.call(Request::TestState)
    }

    /// Test control: act as the human.
    pub fn test_human(self, cmd: HumanCommand) -> Result<(), ClientError> {
        self.call::<IgnoredAny>(Request::TestHuman { cmd }).map(drop)
    }

    fn call<T: DeserializeOwned>(mut self, request: Request) -> Result<T, ClientError> {
        self.send(request)?;
        read_reply(&mut BufReader::new(&self.stream))
    }

    fn send(&mut self, request: Request) -> io::Result<()> {
        let mut line = serde_json::to_string(&Envelope::new(request)).expect("requests serialise");
        line.push('\n');
        self.stream.write_all(line.as_bytes())
    }
}

/// Reads one reply line, for a request whose `ok` carries a `T`.
fn read_reply<T: DeserializeOwned>(reader: &mut impl BufRead) -> Result<T, ClientError> {
    let mut reply = String::new();
    reader.read_line(&mut reply)?;
    if reply.is_empty() {
        return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into());
    }
    let reply: Envelope<Reply> = serde_json::from_str(&reply).map_err(|e| ClientError::Protocol(e.to_string()))?;
    if reply.v != VERSION {
        return Err(ErrorReply::new(
            ErrorCode::Version,
            format!("the running akshually speaks protocol {}, this CLI {VERSION}; restart akshually", reply.v),
        )
        .into());
    }
    match reply.body {
        Reply::Ok(value) => serde_json::from_value(value).map_err(|e| ClientError::Protocol(e.to_string())),
        Reply::Err(error) => Err(error.into()),
    }
}
