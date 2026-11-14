//! Seam B: `akshually update` replacing its own install from a fake GitHub, telling the running
//! app, and what agents' calls see of updates: no release lookups, and a clear error when the
//! app restarts under them.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use akshually_core::UpdateStage;
use akshually_core::ipc::{self, HumanCommand};
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
    let work = TestDirs::new();
    fs::create_dir(work.path().join(&stem)).unwrap();
    fs::write(work.path().join(&stem).join("akshually"), binary).unwrap();
    fs::write(work.path().join(&stem).join("akshually.desktop"), "[Desktop Entry]\n").unwrap();
    (format!("{stem}.tar.gz"), tar_gz(work.path(), &stem))
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
    assert_eq!((notice.version.as_str(), notice.stage), ("99.0.0", UpdateStage::Installed));
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

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn update_refuses_an_unwritable_install_pointing_to_the_install_script() {
    use std::os::unix::fs::PermissionsExt;
    let app = HostedApp::start();
    let github = newer_release("new binary");
    let (dir, exe) = install();
    let before = fs::read(&exe).unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).unwrap();

    let out = update(&app, &exe, &github);

    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(out.status.code(), Some(EXIT_ERROR));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("install.sh"), "{stderr}");
    assert!(fs::read(&exe).unwrap() == before, "the install changed");
}

#[test]
fn no_agent_facing_command_looks_up_releases() {
    let app = HostedApp::start();
    let github = newer_release("new binary");
    let repo = TestRepo::new();
    repo.write("lib.rs", "fn a() {}\n").commit_all("base");
    repo.branch("agent").write("lib.rs", "fn a() {}\nfn b() {}\n");
    let run = |args: &[&str]| {
        let child = app.akshually(repo.path(), args).env("AKSHUALLY_RELEASES_API", github.api()).spawn().unwrap();
        finish(child)
    };

    run(&["start"]);
    run(&["draft", "--lines", "lib.rs:2", "Why b?"]);
    run(&["reply", "T1", "Because."]);
    run(&["resolve", "T1"]);
    run(&["agent-help"]);
    let review = app.akshually(repo.path(), &["review"]).env("AKSHUALLY_RELEASES_API", github.api()).spawn().unwrap();
    let session = app.wait_for_session(repo.path());
    // `draft` made the agent a co-Reviewer, whose co-review the human closes.
    app.service.close(&session.id).unwrap();
    finish(review);

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
