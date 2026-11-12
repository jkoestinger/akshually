//! Fixtures for akshually's tests, shared by seam A (the core review service) and seam B (the
//! CLI contract). Nothing here knows about akshually itself, so both seams can use it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// A real, throwaway git repository on disk (the spec rules out git mocks). Git runs with no
/// global or system config, so tests are hermetic.
pub struct TestRepo {
    _dir: TempDir,
    path: PathBuf,
}

impl TestRepo {
    /// An empty repository whose default branch is `main`.
    pub fn new() -> Self {
        let dir = tempfile::Builder::new().prefix("akrepo").tempdir().expect("create repo dir");
        // Canonical, so it compares equal to what `git rev-parse --show-toplevel` reports
        // (macOS's /tmp is a symlink).
        let path = dir.path().canonicalize().expect("canonicalize repo dir");
        let repo = TestRepo { _dir: dir, path };
        repo.git(&["init", "--quiet", "--initial-branch=main"]);
        repo.git(&["config", "user.name", "Test"]);
        repo.git(&["config", "user.email", "test@example.com"]);
        repo.git(&["config", "commit.gpgsign", "false"]);
        repo
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Writes `contents` to `file` (relative to the repository root), creating directories.
    pub fn write(&self, file: &str, contents: &str) -> &Self {
        self.write_bytes(file, contents.as_bytes())
    }

    /// `write`, for contents that aren't text.
    pub fn write_bytes(&self, file: &str, contents: &[u8]) -> &Self {
        let path = self.path.join(file);
        fs::create_dir_all(path.parent().expect("a file in the repository has a parent dir"))
            .expect("create parent dirs");
        fs::write(path, contents).expect("write file");
        self
    }

    /// Deletes `file` (relative to the repository root) from the working tree.
    pub fn remove(&self, file: &str) -> &Self {
        fs::remove_file(self.path.join(file)).expect("remove file");
        self
    }

    /// Stages everything and commits it.
    pub fn commit_all(&self, message: &str) -> &Self {
        self.git(&["add", "--all"]);
        self.git(&["commit", "--quiet", "--allow-empty", "-m", message]);
        self
    }

    /// Creates `branch` at the current commit and switches to it.
    pub fn branch(&self, branch: &str) -> &Self {
        self.git(&["switch", "--quiet", "-c", branch]);
        self
    }

    /// Switches to the existing `branch`.
    pub fn switch(&self, branch: &str) -> &Self {
        self.git(&["switch", "--quiet", branch]);
        self
    }

    /// Adds a linked worktree of this repository on a new `branch`, created at the current
    /// commit. Keep `self` alive while the worktree is in use.
    pub fn worktree(&self, branch: &str) -> TestRepo {
        let dir = tempfile::Builder::new().prefix("akwt").tempdir().expect("create worktree dir");
        let path = dir.path().canonicalize().expect("canonicalize worktree dir");
        self.git(&["worktree", "add", "--quiet", "-b", branch, path.to_str().expect("UTF-8 temp path")]);
        TestRepo { _dir: dir, path }
    }

    /// Runs git in the repository and returns its trimmed stdout, panicking on failure.
    pub fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(&self.path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("run git");
        assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).expect("git output is UTF-8").trim().to_owned()
    }
}

impl Default for TestRepo {
    fn default() -> Self {
        Self::new()
    }
}

/// A short temporary directory under `/tmp` holding akshually's state and runtime dirs. Short,
/// because a Unix socket path must fit in 104 bytes on macOS, where `$TMPDIR` is long.
pub struct TestDirs {
    root: TempDir,
}

impl TestDirs {
    pub fn new() -> Self {
        let root = tempfile::Builder::new().prefix("ak").tempdir_in("/tmp").expect("create temp dir");
        TestDirs { root }
    }

    pub fn path(&self) -> &Path {
        self.root.path()
    }

    /// akshually's state dir (`AKSHUALLY_STATE_DIR`), not created up front.
    pub fn state(&self) -> PathBuf {
        self.root.path().join("state")
    }

    /// akshually's runtime dir (`AKSHUALLY_RUNTIME_DIR`), holding the socket; not created up front.
    pub fn runtime(&self) -> PathBuf {
        self.root.path().join("run")
    }

    /// The config file to pass as `AKSHUALLY_CONFIG`, so akshually never reads the user's own.
    /// Not created: akshually uses its defaults when there is no such file.
    pub fn config(&self) -> PathBuf {
        self.root.path().join("config.toml")
    }
}

impl Default for TestDirs {
    fn default() -> Self {
        Self::new()
    }
}

/// Polls `check` until it returns `Some`, panicking with `what` after `timeout`.
pub fn wait_until<T>(timeout: Duration, what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A local HTTP server answering each request on its own thread with `answer(path_and_query)`,
/// for what a mock server answering one request at a time can't show: requests made at the
/// same time. Returns its base URL.
pub fn threaded_http_server(answer: impl Fn(&str) -> String + Send + Sync + 'static) -> String {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a local port");
    let url = format!("http://{}", listener.local_addr().expect("the bound address"));
    let answer = std::sync::Arc::new(answer);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let (Ok(stream), answer) = (stream, answer.clone()) else { continue };
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().expect("clone the connection"));
                // One request after another on a kept-alive connection; none has a body.
                loop {
                    let mut request = String::new();
                    if reader.read_line(&mut request).unwrap_or(0) == 0 {
                        return;
                    }
                    let mut header = String::new();
                    while reader.read_line(&mut header).is_ok_and(|n| n > 2) {
                        header.clear();
                    }
                    let target = request.split(' ').nth(1).unwrap_or("/").to_owned();
                    let body = answer(&target);
                    let response = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}", body.len());
                    if (&stream).write_all(response.as_bytes()).is_err() {
                        return;
                    }
                }
            });
        }
    });
    url
}
