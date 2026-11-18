//! Seam B: `akshually update` replacing its own install from a fake GitHub, telling the running
//! app, and what agents' calls see of updates: no release lookups, and a clear error when the
//! app restarts under them.

mod support;

use std::fs;
use std::io::{ErrorKind, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use akshually_core::{
    Anchor, Side,
    ipc::{self, HumanCommand},
};
use akshually_testkit::{FakeRelease, FakeReleases, TestDirs, TestRepo, tar_gz, wait_until};
use support::{HostedApp, LaunchedApp, TIMEOUT, finish};

const EXIT_ERROR: i32 = 1;
/// A tag newer than any version akshually will be built as.
const NEWER: &str = "v99.0.0";
const RUNNING: &str = env!("CARGO_PKG_VERSION");

/// The Linux release asset of `NEWER`, as `packaging/package.sh linux` makes it, whose binary
/// holds `binary`.
fn linux_asset(binary: &str) -> (String, Vec<u8>) {
    let stem = format!("akshually-{NEWER}-x86_64-unknown-linux-gnu");
    let archive = tar_gz(&stem, &[("akshually", binary), ("akshually.desktop", "[Desktop Entry]\n")]);
    (format!("{stem}.tar.gz"), archive)
}

/// A copy of akshually installed in a dir of its own, so updating it leaves the build alone.
fn install() -> (TestDirs, PathBuf) {
    let dir = TestDirs::new();
    let exe = dir.path().join("akshually");
    fs::copy(env!("CARGO_BIN_EXE_akshually"), &exe).unwrap();
    // A child forked by another test thread while the copy was open for writing keeps it "busy"
    // (ETXTBSY, 26) until that child execs.
    wait_until(TIMEOUT, "the copy to be runnable", || match Command::new(&exe).arg("--version").output() {
        Ok(_) => Some(()),
        Err(e) if e.raw_os_error() == Some(26) => None,
        Err(e) => panic!("can't run the copy: {e}"),
    });
    (dir, exe)
}

/// `akshually update` from the copy at `exe`, against `github`.
fn update(app: &HostedApp, exe: &Path, github: &FakeReleases) -> std::process::Output {
    let child = app
        .akshually_at(exe, &std::env::temp_dir(), &["update"])
        .env("AKSHUALLY_RELEASES_API", github.api())
        .spawn()
        .unwrap();
    finish(child)
}

fn newer_release(binary: &str) -> FakeReleases {
    let (name, archive) = linux_asset(binary);
    FakeReleases::new(vec![FakeRelease::new(NEWER).asset(&name, archive)])
}

#[test]
fn update_on_a_current_install_exits_0_and_says_it_is_current() {
    let app = HostedApp::start();
    let github = FakeReleases::new(vec![FakeRelease::new(&format!("v{RUNNING}"))]);
    let (_dir, exe) = install();

    let out = update(&app, &exe, &github);

    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout), format!("akshually {RUNNING} is up to date\n"));
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn update_replaces_the_install_and_tells_the_running_app_to_offer_restart() {
    use akshually_core::UpdateStage;
    let app = HostedApp::start();
    let github = newer_release("new binary");
    let (_dir, exe) = install();

    let out = update(&app, &exe, &github);

    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("updated akshually {RUNNING} → 99.0.0; restart the app to use it\n")
    );
    assert_eq!(fs::read_to_string(&exe).unwrap(), "new binary");
    let notice = app.service.window().update.expect("the app shows no update notice");
    assert_eq!((notice.version.to_string(), notice.stage), ("99.0.0".to_owned(), UpdateStage::Installed));
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn update_refuses_a_hash_mismatch_and_leaves_the_install_untouched() {
    let app = HostedApp::start();
    let (name, archive) = linux_asset("tampered binary");
    let github = FakeReleases::new(vec![FakeRelease::new(NEWER).asset(&name, archive).wrong_hash_for(&name)]);
    let (_dir, exe) = install();
    let before = fs::read(&exe).unwrap();

    let out = update(&app, &exe, &github);

    assert_eq!(out.status.code(), Some(EXIT_ERROR));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("doesn't match its hash in SHA256SUMS"), "{stderr}");
    assert!(fs::read(&exe).unwrap() == before, "the install changed");
    assert_eq!(app.service.window().update, None, "the app was told of an update");
}

#[test]
fn no_agent_facing_command_looks_up_releases() {
    let app = HostedApp::start();
    let github = newer_release("new binary");
    let repo = TestRepo::new();
    repo.write("lib.rs", "fn a() {}\n").commit_all("base");
    let co_review = repo.worktree("co-review");
    co_review.write("lib.rs", "fn a() {}\nfn b() {}\n");
    repo.branch("agent");
    let run = |cwd: &Path, args: &[&str]| {
        let child = app.akshually(cwd, args).env("AKSHUALLY_RELEASES_API", github.api()).spawn().unwrap();
        let out = finish(child);
        assert_eq!(out.status.code(), Some(0), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };

    run(repo.path(), &["start"]);
    // After `start`, so the change is in the Session's Changeset.
    repo.write("lib.rs", "fn a() {}\nfn b() {}\n");
    let review = app.akshually(repo.path(), &["review"]).env("AKSHUALLY_RELEASES_API", github.api()).spawn().unwrap();
    let session = app.wait_for_session(repo.path());
    // The agent answers only Threads a Round brought it.
    app.service.comment(&session.id, Anchor::lines(Side::New, "lib.rs", 1..=1), "Why a?").unwrap();
    app.service.hand_over_round(&session.id).unwrap();
    assert_eq!(finish(review).status.code(), Some(0));
    run(repo.path(), &["reply", "T1", "Because."]);
    run(repo.path(), &["resolve", "T1"]);
    // A Session can't have its Author as a co-Reviewer, so `draft` reviews another branch.
    run(co_review.path(), &["draft", "--lines", "lib.rs:2", "Why b?"]);
    run(repo.path(), &["agent-help"]);

    assert!(!github.looked_up(), "an agent's command looked up releases");
}

#[test]
fn a_review_waiting_when_the_app_restarts_says_so_and_the_session_comes_back() {
    let app = LaunchedApp::new();
    let repo = TestRepo::new();
    repo.write("lib.rs", "fn a() {}\n").commit_all("base");
    repo.branch("agent").write("lib.rs", "fn a() {}\nfn b() {}\n");
    let review = app.akshually(repo.path(), &["review"]).spawn().unwrap();
    let before = app.wait_for("the review to wait", |s| s.sessions.iter().any(|s| s.agent_waiting));

    // The app execs itself, so this request gets no answer.
    let _ = ipc::connect(&app.paths()).unwrap().test_human(HumanCommand::Restart);

    let out = finish(review);
    assert_eq!(out.status.code(), Some(EXIT_ERROR));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("the akshually app restarted") && stderr.contains("run the command again"), "{stderr}");
    let after = app.wait_for("the app to come back", |s| !s.sessions.is_empty());
    assert_eq!(after.pid, before.pid, "the app didn't restart in place");
    assert_eq!(after.sessions[0].id, before.sessions[0].id);
    assert!(!after.sessions[0].agent_waiting, "the restored Session still has an agent waiting");
}

/// A stdout no write can go through, and its other end, which nothing reads: a non-blocking
/// socket whose buffer is already full. Closing a pipe's read end instead isn't enough, as a
/// child spawned meanwhile on another test's thread can inherit it and keep the pipe open; nor
/// is a read-only file, whose `EBADF` Rust's stdout takes for success (#91).
fn full_stdout() -> (Stdio, UnixStream) {
    let (stdout, unread) = UnixStream::pair().unwrap();
    stdout.set_nonblocking(true).unwrap();
    let chunk = [0; 4096];
    loop {
        match (&stdout).write(&chunk) {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(e) => panic!("can't fill the stdout socket: {e}"),
        }
    }
    (Stdio::from(OwnedFd::from(stdout)), unread)
}

#[test]
fn a_review_that_cannot_print_its_outcome_does_not_blame_a_restart() {
    let app = HostedApp::start();
    let repo = TestRepo::new();
    repo.write("lib.rs", "fn a() {}\n").commit_all("base");
    repo.branch("agent").write("lib.rs", "fn a() {}\nfn b() {}\n");
    let (stdout, _unread) = full_stdout();
    let review = app.akshually(repo.path(), &["review"]).stdout(stdout).spawn().unwrap();
    let session = app.wait_for_session(repo.path());

    app.service.approve(&session.id).unwrap();

    let out = finish(review);
    assert_eq!(out.status.code(), Some(EXIT_ERROR));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("can't print the outcome"), "{stderr}");
    assert!(!stderr.contains("restarted"), "a failed print blamed a restart: {stderr}");
}
