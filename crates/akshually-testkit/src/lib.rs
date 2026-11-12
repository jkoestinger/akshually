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
        fs::create_dir_all(path.parent().unwrap()).expect("create parent dirs");
        fs::write(path, contents).expect("write file");
        self
    }

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

    pub fn state(&self) -> PathBuf {
        self.root.path().join("state")
    }

    pub fn runtime(&self) -> PathBuf {
        self.root.path().join("run")
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
