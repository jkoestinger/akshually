//! Seam B harness: run the `akshually` binary as a subprocess against a headless app instance,
//! driving the human side through seam A.
//!
//! - [`HostedApp`] (most tests): the test hosts the review service and its socket in-process,
//!   and acts as the human by calling the service directly. No window, no extra process.
//! - [`LaunchedApp`] (launch and attach only): the CLI spawns a real `akshually app --headless`
//!   (no Rune), whose human side the test drives over the socket's test-control requests.

#![allow(dead_code)] // Each test binary uses part of the harness.

use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::time::Duration;

use akshually_core::forge::{ForgeAdapter, ForgeError, PullRequest, PullRequestRef, RemoteThread, Review};
use akshually_core::ipc::{self, AppState, HumanCommand, Server};
use akshually_core::{Changeset, FileChange, Paths, Service, SessionId, SessionView, Turn, Verdict};
use akshually_testkit::{TestDirs, wait_until};

pub(crate) const TIMEOUT: Duration = Duration::from_secs(10);

/// `akshually args` in `cwd`, with state and runtime dirs and config file from `dirs`, so it never
/// touches the user's own.
fn akshually(dirs: &TestDirs, cwd: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_akshually"));
    cmd.args(args)
        .current_dir(cwd)
        .env("AKSHUALLY_STATE_DIR", dirs.state())
        .env("AKSHUALLY_RUNTIME_DIR", dirs.runtime())
        .env("AKSHUALLY_CONFIG", dirs.config())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

fn paths(dirs: &TestDirs) -> Paths {
    Paths::new(dirs.state(), dirs.runtime())
}

/// An app instance hosted by the test itself: the core review service plus its socket.
pub(crate) struct HostedApp {
    pub dirs: TestDirs,
    pub service: Arc<Service>,
}

impl HostedApp {
    pub(crate) fn start() -> Self {
        Self::serving(Service::new())
    }

    /// An app serving the one Pull Request of [`FakeForge`], at [`PR_URL`].
    pub(crate) fn with_pull_request() -> Self {
        Self::serving(Service::new().with_forges(vec![Box::new(FakeForge)]))
    }

    fn serving(service: Service) -> Self {
        let dirs = TestDirs::new();
        let service = Arc::new(service);
        let server = Server::bind(&paths(&dirs), service.clone()).expect("bind the socket");
        std::thread::spawn(move || server.serve());
        HostedApp { dirs, service }
    }

    pub(crate) fn paths(&self) -> Paths {
        paths(&self.dirs)
    }

    /// `akshually args` in `cwd`, which must find this app rather than launch one.
    pub(crate) fn akshually(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut cmd = akshually(&self.dirs, cwd, args);
        cmd.env("AKSHUALLY_NO_LAUNCH", "1");
        cmd
    }

    /// Waits until a Session for the repository at `repo` is open.
    pub(crate) fn wait_for_session(&self, repo: &Path) -> SessionView {
        wait_until(TIMEOUT, "the Session to open", || {
            self.service.sessions().into_iter().find(|s| s.repo() == Some(repo))
        })
    }

    /// Waits until a `review` call attached to Session `id` after the Round it last received,
    /// so the Session took its Changeset and became the human's Turn again. `agent_waiting`
    /// alone isn't enough: it can still be the call that received the Round, not yet seen gone.
    pub(crate) fn wait_for_review(&self, id: &SessionId) {
        wait_until(TIMEOUT, "the review to attach", || {
            self.service.session(id).filter(|s| s.agent_waiting && s.turn == Turn::Human)
        });
    }

    /// Waits until the co-Reviewer's `review` call waits on the Pull Request's Session.
    pub(crate) fn wait_for_co_reviewer(&self) -> SessionView {
        wait_until(TIMEOUT, "the co-Reviewer to wait", || {
            self.service.sessions().into_iter().find(|s| s.pull_request().is_some() && s.agent_waiting)
        })
    }
}

/// The Pull Request [`FakeForge`] serves.
pub(crate) const PR_URL: &str = "https://forge.test/acme/web/pull/7";

/// A Forge with one Pull Request, `acme/web#7`, changing `login.rs` and `README.md`. Publish
/// accepts any review; nothing goes over the network.
pub(crate) struct FakeForge;

fn pr_ref() -> PullRequestRef {
    PullRequestRef { host: "forge.test".into(), repo: "acme/web".into(), number: 7 }
}

impl ForgeAdapter for FakeForge {
    fn recognise(&self, url: &str) -> Option<PullRequestRef> {
        url.starts_with(PR_URL).then(pr_ref)
    }

    fn pull_request(&self, _: &PullRequestRef) -> Result<PullRequest, ForgeError> {
        Ok(PullRequest {
            id: pr_ref(),
            url: PR_URL.into(),
            title: "Add SSO login".into(),
            author: "maria".into(),
            head_branch: "sso".into(),
            head_sha: "headsha".into(),
            base_sha: "basesha".into(),
        })
    }

    fn changeset(&self, _: &PullRequest) -> Result<Changeset, ForgeError> {
        Ok(Changeset {
            base: "basesha".into(),
            files: vec![
                FileChange::text("login.rs", Some("fn login() {}\n"), Some("fn login() {\n    sso();\n}\n")),
                FileChange::text("README.md", Some("# Web\n"), Some("# Web\n\nSSO.\n")),
            ],
        })
    }

    fn threads(&self, _: &PullRequest) -> Result<Vec<RemoteThread>, ForgeError> {
        Ok(Vec::new())
    }

    fn verdicts(&self, _: &PullRequest) -> Result<Vec<Verdict>, ForgeError> {
        Ok(Verdict::ALL.to_vec())
    }

    fn publish(&self, _: &PullRequest, _: &Review) -> Result<String, ForgeError> {
        Ok(format!("{PR_URL}#pullrequestreview-1"))
    }
}

/// An app the CLI launches itself, headless and with test control on. Killed on drop.
pub(crate) struct LaunchedApp {
    pub dirs: TestDirs,
}

impl LaunchedApp {
    /// No app runs yet: the first `akshually` call launches it.
    pub(crate) fn new() -> Self {
        LaunchedApp { dirs: TestDirs::new() }
    }

    pub(crate) fn akshually(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut cmd = akshually(&self.dirs, cwd, args);
        cmd.env("AKSHUALLY_HEADLESS", "1").env("AKSHUALLY_TEST_CONTROL", "1");
        cmd
    }

    pub(crate) fn paths(&self) -> Paths {
        paths(&self.dirs)
    }

    /// Runs the app itself, headless with test control, allowed at most `limit` open file
    /// descriptors (`ulimit -n`), so the test can make it run out of them. Its stderr is piped.
    pub(crate) fn spawn_app_with_fd_limit(&self, limit: u32) -> AppProcess {
        let app = self.akshually(&std::env::temp_dir(), &["app", "--headless"]);
        let child = Command::new("sh")
            .arg("-c")
            .arg(format!("ulimit -n {limit} && exec \"$0\" \"$@\""))
            .arg(app.get_program())
            .args(app.get_args())
            .envs(app.get_envs().filter_map(|(key, value)| Some((key, value?))))
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the app");
        AppProcess(child)
    }

    /// The running app's pid and Sessions, or `None` while no app answers.
    pub(crate) fn state(&self) -> Option<AppState> {
        ipc::connect(&self.paths()).ok()?.test_state().ok()
    }

    /// Waits until the app reports a state matching `ready`.
    pub(crate) fn wait_for(&self, what: &str, ready: impl Fn(&AppState) -> bool) -> AppState {
        wait_until(TIMEOUT, what, || self.state().filter(|s| ready(s)))
    }

    /// Acts as the human in the running app.
    pub(crate) fn human(&self, cmd: HumanCommand) {
        ipc::connect(&self.paths()).expect("connect").test_human(cmd).expect("human command");
    }
}

impl Drop for LaunchedApp {
    fn drop(&mut self) {
        if let Some(state) = self.state() {
            let _ = Command::new("kill").arg(state.pid.to_string()).status();
        }
    }
}

/// An app process the test spawned itself. Killed and reaped on drop, even when the test fails.
pub(crate) struct AppProcess(pub Child);

impl Drop for AppProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Waits for a spawned CLI call to finish, failing the test if it takes too long.
pub(crate) fn finish(child: Child) -> Output {
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || tx.send(child.wait_with_output()));
    match rx.recv_timeout(TIMEOUT) {
        Ok(output) => output.expect("wait for akshually"),
        Err(_) => {
            let _ = Command::new("kill").arg(pid.to_string()).status();
            panic!("akshually (pid {pid}) didn't exit");
        }
    }
}

/// Whether the spawned CLI call is still blocked.
pub(crate) fn still_running(child: &mut Child) -> bool {
    std::thread::sleep(Duration::from_millis(200));
    child.try_wait().expect("poll akshually").is_none()
}
