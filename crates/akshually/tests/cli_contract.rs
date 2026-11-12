//! Seam B: the CLI contract. Only what seam A can't cover: blocking, stdout, exit codes, and
//! launching or attaching to the single app instance.

mod support;

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;

use akshually_core::Settings;
use akshually_core::ipc::{self, ClientError, ErrorCode, HumanCommand};
use akshually_core::{Anchor, Party, Side, ThreadRef};
use akshually_testkit::{TestDirs, TestRepo, wait_until};
use support::{HostedApp, LaunchedApp, TIMEOUT, finish, still_running};

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
         The human handed over 2 Threads. Address each one, answer it with `akshually reply <thread> \"…\"` \
         or `akshually resolve <thread>`, then run `akshually review` again.\n\
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
fn review_format_json_prints_the_round_as_json() {
    let app = HostedApp::start();
    let repo = repo_with_work();
    let child = app.akshually(repo.path(), &["review", "--format", "json"]).spawn().unwrap();
    let session = app.wait_for_session(repo.path());
    app.service.comment(&session.id, Anchor::lines(Side::New, "lib.rs", 2..=3), "Why two?").unwrap();
    app.service.resolve_thread(&ThreadRef::new(&session.id, &akshually_core::ThreadId::new("T1"))).unwrap();

    app.service.hand_over_round(&session.id).unwrap();

    let out = finish(child);
    assert_eq!(out.status.code(), Some(EXIT_ROUND));
    let printed: serde_json::Value = serde_json::from_slice(&out.stdout).expect("stdout is JSON");
    assert_eq!(
        printed,
        serde_json::json!({
            "outcome": "round",
            "number": 1,
            "threads": [{
                "id": "T1",
                "anchor": {"kind": "lines", "path": "lib.rs", "side": "new", "start": 2, "end": 3},
                "excerpt": "fn b() {}\nfn c() {}\n",
                "comments": [{"author": "human", "body": "Why two?", "round": 1}],
                "resolved": "human",
                "outdated": false,
                "reopened": false,
            }],
        })
    );
}

#[test]
fn review_format_json_prints_the_verdict_as_json() {
    let app = HostedApp::start();
    let repo = repo_with_work();
    let child = app.akshually(repo.path(), &["review", "--format", "json"]).spawn().unwrap();
    let session = app.wait_for_session(repo.path());

    app.service.approve(&session.id).unwrap();

    let out = finish(child);
    assert_eq!(out.status.code(), Some(EXIT_APPROVED));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "{\"outcome\":\"approved\"}\n");
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

/// Hands the `review` call in `repo` a Round with Thread T1, on new line 3.
fn round_1_handed_over(app: &HostedApp, repo: &TestRepo) -> akshually_core::SessionId {
    let call = app.akshually(repo.path(), &["review"]).spawn().unwrap();
    let session = app.wait_for_session(repo.path());
    app.service.comment(&session.id, Anchor::lines(Side::New, "lib.rs", 3..=3), "Drop c").unwrap();
    app.service.hand_over_round(&session.id).unwrap();
    assert_eq!(finish(call).status.code(), Some(EXIT_ROUND));
    session.id
}

#[test]
fn reply_and_resolve_act_on_the_session_for_this_branch_and_return_at_once() {
    let app = HostedApp::start();
    let repo = repo_with_work();
    let session = round_1_handed_over(&app, &repo);

    let replied = finish(app.akshually(repo.path(), &["reply", "T1", "Dropped it."]).spawn().unwrap());
    let resolved = finish(app.akshually(repo.path(), &["resolve", "T1"]).spawn().unwrap());

    assert_eq!(replied.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&replied.stderr));
    assert_eq!(resolved.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&resolved.stderr));
    let thread = &app.service.session(&session).unwrap().threads[0];
    assert_eq!(
        thread.comments.last().map(|c| (c.author.clone(), c.body.as_str())),
        Some((Party::Agent, "Dropped it."))
    );
    assert_eq!(thread.resolved, Some(Party::Agent));
}

#[test]
fn reply_and_resolve_on_an_unknown_thread_exit_1_naming_it() {
    let app = HostedApp::start();
    let repo = repo_with_work();
    round_1_handed_over(&app, &repo);

    for args in [&["reply", "T7", "Done."][..], &["resolve", "T7"]] {
        let out = finish(app.akshually(repo.path(), args).spawn().unwrap());

        assert_eq!(out.status.code(), Some(EXIT_ERROR), "{args:?}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("no Thread T7"), "{args:?}");
    }
}

#[test]
fn review_outside_a_repository_exits_1_with_an_error() {
    let app = HostedApp::start();
    let not_a_repo = TestDirs::new();

    let out = finish(app.akshually(not_a_repo.path(), &["review"]).spawn().unwrap());

    assert_eq!(out.status.code(), Some(EXIT_ERROR));
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not a git repository"), "stderr: {stderr}");
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
    assert!(app.service.sessions().is_empty(), "the call reached an app on another runtime dir");
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

    assert!(second_app.status.success(), "stderr: {}", String::from_utf8_lossy(&second_app.stderr));
    assert_eq!(app.state().map(|s| s.pid), Some(running.pid));
    app.human(HumanCommand::Abandon { session: running.sessions[0].id.clone() });
    assert_eq!(finish(call).status.code(), Some(EXIT_ABANDONED));
}

#[test]
fn an_app_on_the_same_state_dir_with_another_runtime_dir_is_refused_with_a_clear_error() {
    let app = LaunchedApp::new();
    let repo = repo_with_work();
    let call = app.akshually(repo.path(), &["review"]).spawn().unwrap();
    let running = app.wait_for("the Session", |s| s.sessions.len() == 1);
    // As when one CLI runs without XDG_RUNTIME_DIR and another with it.
    let elsewhere = TestDirs::new();

    let other = app.akshually(repo.path(), &["review"]).env("AKSHUALLY_RUNTIME_DIR", elsewhere.runtime()).spawn();
    let out = finish(other.unwrap());

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(EXIT_ERROR), "stderr: {stderr}");
    assert!(stderr.contains("already running"), "stderr: {stderr}");
    assert!(!elsewhere.runtime().join("akshually.sock").exists(), "a second app serves the same Sessions");
    assert_eq!(app.state().map(|s| s.pid), Some(running.pid));
    app.human(HumanCommand::Abandon { session: running.sessions[0].id.clone() });
    assert_eq!(finish(call).status.code(), Some(EXIT_ABANDONED));
}

#[test]
fn the_app_keeps_serving_after_running_out_of_file_descriptors() {
    let app = LaunchedApp::new();
    let mut process = app.spawn_app_with_fd_limit(16);
    let log = BufReader::new(process.0.stderr.take().unwrap());
    let (lines, logged) = mpsc::channel();
    std::thread::spawn(move || {
        let mut emfile = log.lines().map_while(Result::ok).filter(|line| line.contains("os error 24"));
        emfile.try_for_each(|line| lines.send(line))
    });
    app.wait_for("the app to listen", |_| true);

    // Each idle connection holds some of the app's file descriptors until it closes.
    let idle: Vec<_> = (0..32).map(|_| UnixStream::connect(app.paths().socket()).unwrap()).collect();
    let out_of_fds = logged.recv_timeout(TIMEOUT).expect("the app logs running out of file descriptors");
    drop(idle);
    let answered = wait_until(TIMEOUT, "the app to answer again", || app.state());

    assert!(out_of_fds.starts_with("akshually: "), "logged: {out_of_fds}");
    assert_eq!(answered.pid, process.0.id(), "another app answered");
}

#[test]
fn test_control_is_refused_by_an_app_not_started_for_tests() {
    let app = HostedApp::start();

    let refused = ipc::connect(&app.paths()).unwrap().test_state();

    assert!(matches!(refused, Err(ClientError::App(ref e)) if e.code == ErrorCode::Forbidden), "{refused:?}");
}

/// `topic` forks from `main`, and `agent` from `topic`. Each branch adds the file named after
/// it, and `forgot.txt` is untracked.
fn stacked_branches() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("main.txt", "main\n").commit_all("main");
    repo.branch("topic").write("topic.txt", "topic\n").commit_all("topic");
    repo.branch("agent").write("agent.txt", "agent\n").commit_all("agent");
    repo.write("forgot.txt", "forgot\n");
    repo
}

/// Runs `review args` until its Session opens, abandons it, and returns the Session's paths.
fn reviewed_paths(app: &HostedApp, repo: &TestRepo, args: &[&str]) -> Vec<String> {
    let call = app.akshually(repo.path(), &[&["review"], args].concat()).spawn().unwrap();
    let session = app.wait_for_session(repo.path());
    app.service.abandon(&session.id).unwrap();
    assert_eq!(finish(call).status.code(), Some(EXIT_ABANDONED));
    session.shown().files.iter().map(|f| f.path.clone()).collect()
}

#[test]
fn review_takes_a_base_and_the_untracked_override() {
    let app = HostedApp::start();
    let repo = stacked_branches();

    assert_eq!(reviewed_paths(&app, &repo, &[]), ["agent.txt", "forgot.txt", "topic.txt"]);
    assert_eq!(reviewed_paths(&app, &repo, &["--base", "topic", "--no-untracked"]), ["agent.txt"]);
    app.service.set_settings(Settings { untracked: false, ..Settings::default() });
    assert_eq!(reviewed_paths(&app, &repo, &["--base", "topic", "--untracked"]), ["agent.txt", "forgot.txt"]);
}

#[test]
fn review_takes_a_commit_range() {
    let app = HostedApp::start();
    let repo = stacked_branches();

    assert_eq!(reviewed_paths(&app, &repo, &["main..topic"]), ["topic.txt"]);
}

#[test]
fn review_refuses_a_base_together_with_a_range() {
    let app = HostedApp::start();
    let repo = stacked_branches();

    let out = finish(app.akshually(repo.path(), &["review", "--base", "main", "main..topic"]).spawn().unwrap());

    assert_eq!(out.status.code(), Some(2), "a usage error");
    assert!(app.service.sessions().is_empty(), "a refused review opened a Session");
}

#[test]
fn start_records_the_session_base_and_returns_at_once() {
    let app = HostedApp::start();
    let repo = stacked_branches();

    let started = finish(app.akshually(repo.path(), &["start"]).spawn().unwrap());
    repo.write("later.txt", "after start\n");

    assert_eq!(started.status.code(), Some(0));
    assert!(!started.stdout.is_empty(), "start says what it did");
    assert_eq!(reviewed_paths(&app, &repo, &[]), ["later.txt"]);
}

#[test]
fn the_app_reads_the_untracked_setting_from_its_config_file() {
    let app = LaunchedApp::new();
    let repo = stacked_branches();
    let config = app.dirs.config();
    std::fs::write(&config, "untracked = false\n").unwrap();

    let call = app.akshually(repo.path(), &["review"]).env("AKSHUALLY_CONFIG", &config).spawn().unwrap();
    let state = app.wait_for("the Session", |s| s.sessions.len() == 1);

    let paths: Vec<&str> = state.sessions[0].changeset.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["agent.txt", "topic.txt"]);
    app.human(HumanCommand::Abandon { session: state.sessions[0].id.clone() });
    assert_eq!(finish(call).status.code(), Some(EXIT_ABANDONED));
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

#[test]
fn opening_the_app_again_shows_the_running_ones_window_with_the_activation_token() {
    let app = HostedApp::start();
    let shown = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = shown.clone();
    app.service.on_show_window(move |token| sink.lock().unwrap().push(token));

    let out = app.akshually(&std::env::temp_dir(), &[]).env("XDG_ACTIVATION_TOKEN", "launcher-token").output().unwrap();
    let again = app.akshually(&std::env::temp_dir(), &[]).env_remove("XDG_ACTIVATION_TOKEN").output().unwrap();

    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(again.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&again.stderr));
    assert_eq!(*shown.lock().unwrap(), vec![Some("launcher-token".to_owned()), None]);
}

#[test]
fn a_second_review_while_one_waits_exits_1_with_a_clear_message() {
    let app = HostedApp::start();
    let repo = repo_with_work();
    let mut first = app.akshually(repo.path(), &["review"]).spawn().unwrap();
    let session = app.wait_for_session(repo.path());

    let second = finish(app.akshually(repo.path(), &["review"]).spawn().unwrap());

    assert_eq!(second.status.code(), Some(EXIT_ERROR));
    assert!(second.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&second.stderr),
        "akshually: another `akshually review` is already waiting on this Session\n"
    );
    assert!(still_running(&mut first), "the first call stopped waiting");
    app.service.approve(&session.id).unwrap();
    assert_eq!(finish(first).status.code(), Some(EXIT_APPROVED));
}

#[test]
fn a_round_handed_over_after_the_agent_was_killed_goes_to_the_next_review() {
    let app = HostedApp::start();
    let repo = repo_with_work();
    let mut first = app.akshually(repo.path(), &["review"]).spawn().unwrap();
    let session = app.wait_for_session(repo.path());

    first.kill().unwrap();
    first.wait().unwrap();
    wait_until(TIMEOUT, "the app to notice", || app.service.session(&session.id).filter(|s| !s.agent_waiting));
    app.service.comment(&session.id, Anchor::lines(Side::New, "lib.rs", 3..=3), "Drop c").unwrap();
    app.service.hand_over_round(&session.id).unwrap();

    let out = finish(app.akshually(repo.path(), &["review"]).spawn().unwrap());

    assert_eq!(out.status.code(), Some(EXIT_ROUND));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("# Round 1\n"), "{stdout}");
    let mut third = app.akshually(repo.path(), &["review"]).spawn().unwrap();
    assert!(still_running(&mut third), "the Round was delivered twice");
    app.service.approve(&session.id).unwrap();
    assert_eq!(finish(third).status.code(), Some(EXIT_APPROVED));
}

#[test]
fn a_verdict_given_while_no_review_waits_goes_to_the_next_review() {
    let app = HostedApp::start();
    let repo = repo_with_work();
    let mut first = app.akshually(repo.path(), &["review"]).spawn().unwrap();
    let session = app.wait_for_session(repo.path());
    first.kill().unwrap();
    first.wait().unwrap();
    wait_until(TIMEOUT, "the app to notice", || app.service.session(&session.id).filter(|s| !s.agent_waiting));

    app.service.approve(&session.id).unwrap();

    let out = finish(app.akshually(repo.path(), &["review"]).spawn().unwrap());
    assert_eq!(out.status.code(), Some(EXIT_APPROVED));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "Approved.\n");
    assert!(app.service.sessions().is_empty(), "collected, the Session ends");
}
