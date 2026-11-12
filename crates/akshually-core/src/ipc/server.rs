use std::fs::{self, DirBuilder, File, OpenOptions, TryLockError};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;

use super::proto::{AppState, Envelope, ErrorCode, ErrorReply, HumanCommand, Reply, Request, VERSION};
use crate::{Paths, ReviewRequest, Service};

#[derive(Debug, thiserror::Error)]
pub enum BindError {
    #[error("another akshually app is already running")]
    AlreadyRunning,
    #[error("could not open akshually's socket: {0}")]
    Io(#[from] io::Error),
}

/// The app's end of the local channel: serves CLI requests against one review service.
pub struct Server {
    listener: UnixListener,
    service: Arc<Service>,
    test_control: bool,
    // Held for the life of the process; the kernel releases it on exit, crash included.
    _lock: File,
}

impl Server {
    /// Takes the single-instance lock and listens on the socket. Fails with `AlreadyRunning`
    /// when another app holds the lock.
    pub fn bind(paths: &Paths, service: Arc<Service>) -> Result<Server, BindError> {
        DirBuilder::new().recursive(true).mode(0o700).create(&paths.runtime)?;
        let lock = OpenOptions::new().create(true).truncate(false).write(true).open(paths.lock())?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(BindError::AlreadyRunning),
            Err(TryLockError::Error(e)) => return Err(e.into()),
        }
        // Holding the lock, any socket file left is stale (from a crash).
        match fs::remove_file(paths.socket()) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        let listener = UnixListener::bind(paths.socket())?;
        Ok(Server { listener, service, test_control: false, _lock: lock })
    }

    /// Accepts the `test.*` requests, which let tests drive the human side over the socket.
    pub fn with_test_control(mut self) -> Self {
        self.test_control = true;
        self
    }

    /// Serves requests until the process exits, one thread per connection (a `review` blocks
    /// its thread until the human answers).
    pub fn serve(self) -> io::Result<()> {
        let server = Arc::new(self);
        for stream in server.listener.incoming() {
            let stream = stream?;
            let server = server.clone();
            std::thread::spawn(move || server.handle(stream));
        }
        Ok(())
    }

    fn handle(&self, stream: UnixStream) {
        let mut line = String::new();
        if BufReader::new(&stream).read_line(&mut line).is_err() {
            return;
        }
        let reply = match parse(&line) {
            Ok(request) => self.answer(request),
            Err(error) => Reply::Err(error),
        };
        let mut out = serde_json::to_string(&Envelope::new(reply)).expect("replies serialize");
        out.push('\n');
        // The client may have gone away while waiting; nothing to do about it.
        let _ = (&stream).write_all(out.as_bytes());
    }

    fn answer(&self, request: Request) -> Reply {
        let result = match request {
            Request::Review { cwd } => self.review(cwd),
            Request::Reply { cwd, thread, body } => {
                self.service.reply(&cwd, &thread, body).map(|()| serde_json::json!({})).map_err(failed)
            }
            Request::Resolve { cwd, thread } => {
                self.service.resolve(&cwd, &thread).map(|()| serde_json::json!({})).map_err(failed)
            }
            Request::TestState | Request::TestHuman { .. } if !self.test_control => {
                Err(ErrorReply::new(ErrorCode::Forbidden, "this app was not started for tests"))
            }
            Request::TestState => Ok(json(AppState { pid: std::process::id(), sessions: self.service.sessions() })),
            Request::TestHuman { cmd } => self.human(cmd),
        };
        match result {
            Ok(value) => Reply::Ok(value),
            Err(error) => Reply::Err(error),
        }
    }

    fn review(&self, cwd: std::path::PathBuf) -> Result<serde_json::Value, ErrorReply> {
        let pending = self.service.review(ReviewRequest::new(cwd)).map_err(failed)?;
        match pending.wait() {
            Some(outcome) => Ok(json(outcome)),
            None => Err(ErrorReply::new(ErrorCode::Failed, "the Session stopped waiting for this call")),
        }
    }

    fn human(&self, cmd: HumanCommand) -> Result<serde_json::Value, ErrorReply> {
        match cmd {
            HumanCommand::Approve { session } => self.service.approve(&session),
            HumanCommand::Abandon { session } => self.service.abandon(&session),
        }
        .map_err(failed)?;
        Ok(serde_json::json!({}))
    }
}

fn parse(line: &str) -> Result<Request, ErrorReply> {
    let bad = |e: serde_json::Error| ErrorReply::new(ErrorCode::BadRequest, e.to_string());
    let value: serde_json::Value = serde_json::from_str(line).map_err(bad)?;
    // Check the version before the body, whose shape may differ between versions.
    let v = value.get("v").and_then(serde_json::Value::as_u64);
    if v != Some(u64::from(VERSION)) {
        return Err(ErrorReply::new(
            ErrorCode::Version,
            format!("this akshually speaks protocol {VERSION}; restart akshually"),
        ));
    }
    let envelope: Envelope<Request> = serde_json::from_value(value).map_err(bad)?;
    Ok(envelope.body)
}

fn failed(error: crate::Error) -> ErrorReply {
    ErrorReply::new(ErrorCode::Failed, error.to_string())
}

fn json(value: impl serde::Serialize) -> serde_json::Value {
    serde_json::to_value(value).expect("replies serialize")
}
