use std::fs::{self, DirBuilder, File, OpenOptions, TryLockError};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, mpsc};

use super::proto::{AppState, Envelope, ErrorCode, ErrorReply, HumanCommand, Reply, Request, VERSION};
use crate::{NewComment, Paths, ReviewRequest, Service};

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
        let Ok(read) = stream.try_clone() else { return };
        let mut reader = BufReader::new(read);
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            return;
        }
        let reply = match parse(&line) {
            Ok(Request::Review { cwd, local, pr }) => {
                let request = ReviewRequest { cwd, local, ack: true, pull_request: pr };
                return self.review(request, &stream, reader);
            }
            Ok(request) => self.answer(request),
            Err(error) => Reply::Err(error),
        };
        // The client may have gone away while waiting; nothing to do about it.
        let _ = send(&stream, reply);
    }

    /// Serves a `review` call: waits for the human while watching the connection, so a CLI
    /// that goes away frees the Session, and hands a Round over for good only once the CLI
    /// acknowledges it.
    fn review(&self, request: ReviewRequest, stream: &UnixStream, mut reader: BufReader<UnixStream>) {
        let pending = match self.service.review(request) {
            Ok(pending) => pending,
            Err(error) => {
                let _ = send(stream, Reply::Err(failed(error)));
                return;
            }
        };
        // The CLI's next line is the ack; EOF before it means the CLI went away.
        let (next_line, client) = mpsc::channel();
        let cancel = pending.canceller();
        std::thread::spawn(move || {
            let mut line = String::new();
            let read = matches!(reader.read_line(&mut line), Ok(n) if n > 0);
            if !read {
                cancel.cancel();
            }
            let _ = next_line.send(read.then_some(line));
        });

        let Some(outcome) = pending.receive() else {
            let _ = send(stream, Reply::Err(ErrorReply::new(ErrorCode::Failed, "stopped waiting for the human")));
            return;
        };
        if send(stream, Reply::Ok(json(outcome))).is_err() {
            return;
        }
        // Without the ack, dropping `pending` queues the Round or Verdict again.
        if let Ok(Some(line)) = client.recv()
            && matches!(parse(&line), Ok(Request::Ack))
        {
            pending.ack();
            let _ = send(stream, Reply::Ok(serde_json::json!({})));
        }
    }

    fn answer(&self, request: Request) -> Reply {
        let result = match request {
            Request::Review { .. } | Request::Ack => {
                Err(ErrorReply::new(ErrorCode::BadRequest, "`review` and `ack` need a connection of their own"))
            }
            Request::Start { cwd } => self.service.start(&cwd).map(|()| serde_json::json!({})).map_err(failed),
            Request::Reply { cwd, thread, body } => {
                self.service.reply(&cwd, &thread, body).map(|()| serde_json::json!({})).map_err(failed)
            }
            Request::Resolve { cwd, thread } => {
                self.service.resolve(&cwd, &thread).map(|()| serde_json::json!({})).map_err(failed)
            }
            Request::Draft { pr, anchor, body, suggestion } => self
                .service
                .draft(&pr, anchor, NewComment { body, suggestion })
                .map(|thread| serde_json::json!({ "thread": thread }))
                .map_err(failed),
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

fn send(stream: &UnixStream, reply: Reply) -> io::Result<()> {
    let mut out = serde_json::to_string(&Envelope::new(reply)).expect("replies serialize");
    out.push('\n');
    (&*stream).write_all(out.as_bytes())
}

fn failed(error: crate::Error) -> ErrorReply {
    let code = match error {
        crate::Error::Busy => ErrorCode::Busy,
        _ => ErrorCode::Failed,
    };
    ErrorReply::new(code, error.to_string())
}

fn json(value: impl serde::Serialize) -> serde_json::Value {
    serde_json::to_value(value).expect("replies serialize")
}
