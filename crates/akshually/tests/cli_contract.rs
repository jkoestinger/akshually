//! Seam B: the CLI contract. Only what seam A can't cover: blocking, stdout, exit codes, and
//! launching or attaching to the single app instance.

mod support;

use akshually_core::ipc::{self, ClientError, ErrorCode, HumanCommand};
use akshually_testkit::{TestDirs, TestRepo};
use support::{HostedApp, LaunchedApp, finish, still_running};

const EXIT_APPROVED: i32 = 0;
const EXIT_ERROR: i32 = 1;
const EXIT_ABANDONED: i32 = 3;

fn repo_with_work() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("lib.rs", "fn a() {}\n").commit_all("base");
    repo.branch("agent").write("lib.rs", "fn a() {}\nfn b() {}\n").commit_all("add b");
    repo.write("lib.rs", "fn a() {}\nfn b() {}\nfn c() {}\n");
    repo
}

#[test]
fn review_blocks_until_the_human_approves_then_exits_0() {
    let app = HostedApp::start();
    let repo = repo_with_work();
    let mut child = app.akshually(repo.path(), &["review"]).spawn().unwrap();
    let session = app.wait_for_session(repo.path());

    assert!(still_running(&mut child), "review returned before the human acted");
    app.service.approve(&session.id).unwrap();

    let out = finish(child);
    assert_eq!(out.status.code(), Some(EXIT_APPROVED));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "Approved.\n");
}

#[test]
fn review_exits_with_the_abandoned_code_when_the_human_abandons() {
    let app = HostedApp::start();
    let repo = repo_with_work();
    let child = app.akshually(repo.path(), &["review"]).spawn().unwrap();
    let session = app.wait_for_session(repo.path());

    app.service.abandon(&session.id).unwrap();

    let out = finish(child);
    assert_eq!(out.status.code(), Some(EXIT_ABANDONED));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "Abandoned: the human closed this Session without a Verdict. Stop working on this change.\n"
    );
}

#[test]
fn review_outside_a_repository_exits_1_with_an_error() {
    let app = HostedApp::start();
    let not_a_repo = TestDirs::new();

    let out = finish(app.akshually(not_a_repo.path(), &["review"]).spawn().unwrap());

    assert_eq!(out.status.code(), Some(EXIT_ERROR));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a git repository"));
}

#[test]
fn review_fails_when_no_app_runs_and_launching_is_off() {
    let app = HostedApp::start();
    let elsewhere = TestDirs::new();
    let repo = repo_with_work();

    let out = finish(
        app.akshually(repo.path(), &["review"]).env("AKSHUALLY_RUNTIME_DIR", elsewhere.runtime()).spawn().unwrap(),
    );

    assert_eq!(out.status.code(), Some(EXIT_ERROR));
    assert!(app.service.sessions().is_empty());
}

#[test]
fn first_review_launches_the_app_and_a_second_attaches_to_it() {
    let app = LaunchedApp::new();
    let (first, second) = (repo_with_work(), repo_with_work());

    let first_call = app.akshually(first.path(), &["review"]).spawn().unwrap();
    let launched = app.wait_for("the first Session", |s| s.sessions.len() == 1);
    let second_call = app.akshually(second.path(), &["review"]).spawn().unwrap();
    let attached = app.wait_for("the second Session", |s| s.sessions.len() == 2);

    assert_eq!(attached.pid, launched.pid, "the second call started another app");
    for session in &attached.sessions {
        app.human(HumanCommand::Approve { session: session.id.clone() });
    }
    assert_eq!(finish(first_call).status.code(), Some(EXIT_APPROVED));
    assert_eq!(finish(second_call).status.code(), Some(EXIT_APPROVED));
}

#[test]
fn a_second_app_exits_and_leaves_the_running_one_serving() {
    let app = LaunchedApp::new();
    let repo = repo_with_work();
    let call = app.akshually(repo.path(), &["review"]).spawn().unwrap();
    let running = app.wait_for("the Session", |s| s.sessions.len() == 1);

    let second_app = finish(app.akshually(repo.path(), &["app", "--headless"]).spawn().unwrap());

    assert!(second_app.status.success());
    assert_eq!(app.state().map(|s| s.pid), Some(running.pid));
    app.human(HumanCommand::Abandon { session: running.sessions[0].id.clone() });
    assert_eq!(finish(call).status.code(), Some(EXIT_ABANDONED));
}

#[test]
fn test_control_is_refused_by_an_app_not_started_for_tests() {
    let app = HostedApp::start();

    let refused = ipc::connect(&app.paths()).unwrap().test_state();

    assert!(matches!(refused, Err(ClientError::App(e)) if e.code == ErrorCode::Forbidden));
}

#[test]
fn version_prints_the_package_version() {
    let app = HostedApp::start();

    let out = app.akshually(&std::env::temp_dir(), &["--version"]).output().unwrap();

    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), format!("akshually {}\n", env!("CARGO_PKG_VERSION")));
}

/// The `.desktop` entry and the macOS `.app` start the binary with no arguments. That opens the
/// app, which leaves the CLI to the instance already running.
#[test]
fn no_subcommand_opens_the_app_and_defers_to_a_running_one() {
    let app = HostedApp::start();

    let out = app.akshually(&std::env::temp_dir(), &[]).output().unwrap();

    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
}
