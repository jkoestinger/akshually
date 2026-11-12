//! Seam B: the CLI contract. Only what seam A can't cover: blocking, stdout, exit codes, and
//! launching or attaching to the single app instance.

mod support;

use akshually_core::ipc::{self, ClientError, ErrorCode, HumanCommand};
use akshually_core::{Anchor, Side};
use akshually_testkit::{TestDirs, TestRepo};
use support::{HostedApp, LaunchedApp, finish, still_running};

const EXIT_APPROVED: i32 = 0;
const EXIT_ERROR: i32 = 1;
const EXIT_ABANDONED: i32 = 3;
const EXIT_ROUND: i32 = 4;

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
fn review_prints_the_handed_over_round_as_markdown_and_exits_with_the_round_code() {
    let app = HostedApp::start();
    let repo = repo_with_work();
    let mut child = app.akshually(repo.path(), &["review"]).spawn().unwrap();
    let session = app.wait_for_session(repo.path());
    app.service.comment(&session.id, Anchor::lines(Side::New, "lib.rs", 2..=3), "Why two?\n\n- one is enough").unwrap();
    app.service.comment(&session.id, Anchor::lines(Side::Old, "lib.rs", 1..=1), "Keep `a`.").unwrap();

    assert!(still_running(&mut child), "a Draft reached the agent before the handover");
    app.service.hand_over_round(&session.id).unwrap();

    let out = finish(child);
    assert_eq!(out.status.code(), Some(EXIT_ROUND));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "# Round 1\n\
         \n\
         The human handed over 2 Threads. Address each one, then run `akshually review` again.\n\
         \n\
         ## T1 · lib.rs, new lines 2-3\n\
         \n\
         ```\n\
         fn b() {}\n\
         fn c() {}\n\
         ```\n\
         \n\
         Why two?\n\
         \n\
         - one is enough\n\
         \n\
         ## T2 · lib.rs, old line 1\n\
         \n\
         ```\n\
         fn a() {}\n\
         ```\n\
         \n\
         Keep `a`.\n"
    );
}

#[test]
fn the_next_review_attaches_to_the_same_session_and_keeps_its_threads() {
    let app = HostedApp::start();
    let repo = repo_with_work();
    let first = app.akshually(repo.path(), &["review"]).spawn().unwrap();
    let session = app.wait_for_session(repo.path());
    app.service.comment(&session.id, Anchor::lines(Side::New, "lib.rs", 3..=3), "Drop c").unwrap();
    app.service.hand_over_round(&session.id).unwrap();
    assert_eq!(finish(first).status.code(), Some(EXIT_ROUND));

    repo.write("lib.rs", "fn a() {}\nfn b() {}\n");
    let mut second = app.akshually(repo.path(), &["review"]).spawn().unwrap();

    assert!(still_running(&mut second), "the second call didn't wait for the human");
    let sessions = app.service.sessions();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, session.id);
    assert_eq!(sessions[0].round, 2);
    assert_eq!(sessions[0].threads.len(), 1);
    assert_eq!(sessions[0].changeset.files[0].new.as_deref(), Some("fn a() {}\nfn b() {}\n"));
    app.service.approve(&session.id).unwrap();
    assert_eq!(finish(second).status.code(), Some(EXIT_APPROVED));
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
