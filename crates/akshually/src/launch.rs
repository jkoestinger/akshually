//! Reaching the single app instance from a CLI call, launching it first if none runs.

use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use akshually_core::Paths;
use akshually_core::ipc::{self, Connection};

/// How long a CLI call waits for the app it launched to listen before giving up.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(10);
/// The first wait between tries to reach a launched app, doubling up to [`MAX_RETRY`], so a
/// quick start is noticed at once without spinning through a slow one.
const FIRST_RETRY: Duration = Duration::from_millis(10);
/// The longest wait between tries to reach a launched app.
const MAX_RETRY: Duration = Duration::from_millis(200);

/// Connects to the running app, launching it when none answers.
///
/// Environment: `AKSHUALLY_NO_LAUNCH` fails instead of launching; `AKSHUALLY_HEADLESS` launches
/// the app without UI (tests).
pub(crate) fn connect_or_launch(paths: &Paths) -> Result<Connection> {
    match ipc::connect(paths) {
        Err(e) if no_app(&e) => {}
        other => return other.context("can't reach the akshually app"),
    }
    if std::env::var_os("AKSHUALLY_NO_LAUNCH").is_some() {
        bail!("the akshually app isn't running, and AKSHUALLY_NO_LAUNCH is set");
    }
    let mut app = launch(paths)?;

    // Two racing CLI calls may both launch an app; the loser exits, and both connect to the winner.
    let deadline = Instant::now() + LAUNCH_TIMEOUT;
    let mut delay = FIRST_RETRY;
    loop {
        match ipc::connect(paths) {
            Err(e) if no_app(&e) && Instant::now() < deadline => {
                // The app gave up: it says why last in its log.
                if let Ok(Some(status)) = app.try_wait()
                    && !status.success()
                {
                    let log = fs::read_to_string(paths.app_log()).unwrap_or_default();
                    let why = log.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no reason given");
                    bail!("the akshually app didn't start: {why}");
                }
                std::thread::sleep(delay);
                delay = (delay * 2).min(MAX_RETRY);
            }
            other => {
                return other
                    .with_context(|| format!("the akshually app didn't start; see {}", paths.app_log().display()));
            }
        }
    }
}

fn no_app(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused)
}

/// Starts `akshually app` detached from this call: its own process group (so a Ctrl-C in the
/// agent's terminal doesn't reach it), no stdin, output to the app log.
fn launch(paths: &Paths) -> Result<Child> {
    fs::create_dir_all(&paths.state).with_context(|| format!("can't create {}", paths.state.display()))?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.app_log())
        .with_context(|| format!("can't open {}", paths.app_log().display()))?;
    // Canonical, so on macOS the PATH symlink resolves into the app bundle.
    let exe = std::env::current_exe().and_then(|p| p.canonicalize()).context("can't find the akshually binary")?;
    let mut cmd = Command::new(exe);
    cmd.arg("app");
    if std::env::var_os("AKSHUALLY_HEADLESS").is_some() {
        cmd.arg("--headless");
    }
    cmd.stdin(Stdio::null())
        .stdout(log.try_clone().with_context(|| format!("can't open {}", paths.app_log().display()))?)
        .stderr(log)
        .process_group(0)
        .spawn()
        .context("can't launch the akshually app")
}
