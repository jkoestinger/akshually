use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use serde::de::{DeserializeOwned, IgnoredAny};

use super::proto::{AppState, Envelope, ErrorCode, ErrorReply, HumanCommand, Reply, Request, VERSION};
use crate::{LocalOptions, Outcome, Paths};

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
    /// Asks for review of the local work in `cwd` and blocks until the human answers.
    pub fn review(self, cwd: &Path, local: LocalOptions) -> Result<Outcome, ClientError> {
        self.call(Request::Review { cwd: cwd.to_owned(), local })
    }

    /// Records the Session base for the local work in `cwd`.
    pub fn start(self, cwd: &Path) -> Result<(), ClientError> {
        self.call::<IgnoredAny>(Request::Start { cwd: cwd.to_owned() }).map(drop)
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
        let mut line = serde_json::to_string(&Envelope::new(request)).expect("requests serialize");
        line.push('\n');
        self.stream.write_all(line.as_bytes())?;

        let mut reply = String::new();
        BufReader::new(&self.stream).read_line(&mut reply)?;
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
}
