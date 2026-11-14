//! Seam A: the update notice in the core review service: checking for a newer release, installing
//! it, and what Restart has to warn about, against a fake GitHub.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use akshually_core::update::{Archive, Releases, Target, Updater, Version};
use akshually_core::{ReviewRequest, Service, Settings, UpdateStage, UpdateView};
use akshually_testkit::{FakeRelease, FakeReleases, TestRepo, tar_gz};

/// A tag newer than any version akshually will be built as.
const NEWER: &str = "v99.0.0";
/// The platform the tests update, whichever they run on.
const TARGET: &str = "x86_64-unknown-linux-gnu";

/// `TARGET`'s release asset of `NEWER`, holding `binary`.
fn asset(binary: &str) -> (String, Vec<u8>) {
    let stem = format!("akshually-{NEWER}-{TARGET}");
    let work = tempfile::tempdir().unwrap();
    fs::create_dir(work.path().join(&stem)).unwrap();
    fs::write(work.path().join(&stem).join("akshually"), binary).unwrap();
    (format!("{stem}.tar.gz"), tar_gz(work.path(), &stem))
}

/// An installed binary, in a dir of its own.
fn install() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("akshually");
    fs::write(&exe, "old binary").unwrap();
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
    (dir, exe)
}

/// A service updating the install `exe` from `github`.
fn service(github: &FakeReleases, exe: PathBuf) -> Service {
    Service::new().with_updater(updater(&github.api(), exe))
}

fn updater(api: &str, exe: PathBuf) -> Updater {
    Updater::new(Releases::new(api), exe).target(Target::new(TARGET, Archive::TarGz))
}

fn newer_release() -> FakeReleases {
    let (name, archive) = asset("new binary");
    FakeReleases::new(vec![FakeRelease::new(NEWER).asset(&name, archive)])
}

fn stage(svc: &Service) -> Option<UpdateStage> {
    svc.window().update.map(|u| u.stage)
}

#[test]
fn a_newer_release_shows_the_update_notice_naming_it() {
    let github = newer_release();
    let (_dir, exe) = install();
    let svc = service(&github, exe);

    svc.check_for_updates();

    let update = svc.window().update.expect("no update notice");
    assert_eq!(update, UpdateView { version: "99.0.0".into(), stage: UpdateStage::Available, waiting: Vec::new() });
}

#[test]
fn a_current_install_shows_no_notice() {
    let current = format!("v{}", Version::running());
    let github = FakeReleases::new(vec![FakeRelease::new(&current)]);
    let (_dir, exe) = install();
    let svc = service(&github, exe);

    svc.check_for_updates();

    assert_eq!(svc.window().update, None);
}

#[test]
fn with_check_for_updates_off_the_app_makes_no_release_request() {
    let github = newer_release();
    let (_dir, exe) = install();
    let svc = service(&github, exe);
    svc.set_settings(Settings { check_for_updates: false, ..Settings::default() });

    svc.check_for_updates();

    assert!(!github.looked_up(), "the releases were looked up");
    assert_eq!(svc.window().update, None);
}

#[test]
fn a_failed_check_shows_no_notice() {
    let mut github = mockito::Server::new();
    github.mock("GET", mockito::Matcher::Any).with_status(500).create();
    let (_dir, exe) = install();
    let svc = Service::new().with_updater(updater(&github.url(), exe));

    svc.check_for_updates();

    assert_eq!(svc.window().update, None);
}

#[test]
fn a_dismissed_notice_stays_hidden_through_later_checks() {
    let github = newer_release();
    let (_dir, exe) = install();
    let svc = service(&github, exe);
    svc.check_for_updates();

    svc.dismiss_update();
    svc.check_for_updates();

    assert_eq!(svc.window().update, None);
}

#[test]
fn updating_installs_the_release_then_offers_restart() {
    let github = newer_release();
    let (_dir, exe) = install();
    let svc = service(&github, exe.clone());
    svc.check_for_updates();

    svc.install_update().unwrap();

    assert_eq!(stage(&svc), Some(UpdateStage::Installed));
    assert_eq!(fs::read_to_string(exe).unwrap(), "new binary");
}

#[test]
fn an_update_that_fails_says_why_and_can_be_tried_again() {
    let (name, archive) = asset("tampered binary");
    let github = FakeReleases::new(vec![FakeRelease::new(NEWER).asset(&name, archive).wrong_hash_for(&name)]);
    let (_dir, exe) = install();
    let svc = service(&github, exe.clone());
    svc.check_for_updates();

    assert!(svc.install_update().is_err(), "a tampered release installed");

    let Some(UpdateStage::Failed(why)) = stage(&svc) else { panic!("the notice doesn't show the failure") };
    assert!(why.contains("SHA256SUMS"), "{why}");
    assert_eq!(fs::read_to_string(exe).unwrap(), "old binary");
}

#[test]
fn an_update_installed_from_the_cli_offers_restart_even_after_a_dismissal() {
    let github = newer_release();
    let (_dir, exe) = install();
    let svc = service(&github, exe);
    svc.check_for_updates();
    svc.dismiss_update();

    svc.update_installed("99.0.0");

    let update = svc.window().update.expect("no update notice");
    assert_eq!((update.version.as_str(), update.stage), ("99.0.0", UpdateStage::Installed));
}

#[test]
fn the_notice_names_the_sessions_whose_agents_wait_so_restart_can_warn_about_them() {
    let repo = TestRepo::new();
    repo.write("lib.rs", "fn a() {}\n").commit_all("base");
    repo.branch("agent").write("lib.rs", "fn b() {}\n");
    let svc = Service::new();
    let _waiting = svc.review(ReviewRequest::new(repo.path())).unwrap();
    let title = svc.sessions()[0].title();

    svc.update_installed("99.0.0");

    assert_eq!(svc.window().update.expect("no update notice").waiting, [title]);
}

#[test]
fn restart_relaunches_the_app_through_the_registered_relauncher() {
    let svc = Service::new();
    let (tx, rx) = std::sync::mpsc::channel();
    svc.on_restart(move || {
        tx.send(()).unwrap();
        std::io::Error::other("relaunched")
    });

    let error = svc.restart().unwrap_err();

    assert!(rx.try_recv().is_ok(), "the relauncher wasn't called");
    assert!(error.to_string().contains("relaunched"), "{error}");
}
