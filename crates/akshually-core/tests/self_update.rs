//! Seam C: self-update at its HTTP boundary: which release is offered, looking releases up and
//! downloading them from a `mockito` GitHub, and replacing a throwaway install with one.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use akshually_core::update::{
    Archive, Asset, Release, Releases, Target, UpdateError, Updater, Version, release_offered,
};
use akshually_testkit::{FakeRelease, FakeReleases, TestDirs, tar_gz, write_executable, zip};

fn release(tag: &str) -> Release {
    Release { tag: tag.into(), version: tag.parse().unwrap(), assets: Vec::new() }
}

fn offered_tag(running: &str, tags: &[&str]) -> Option<String> {
    let releases: Vec<Release> = tags.iter().map(|t| release(t)).collect();
    release_offered(&running.parse().unwrap(), &releases).map(|r| r.tag.clone())
}

#[test]
fn a_pre_release_comes_before_its_stable_release() {
    let rc: Version = "0.1.0-rc.1".parse().unwrap();
    let stable: Version = "0.1.0".parse().unwrap();

    assert!(rc < stable, "0.1.0-rc.1 sorts after 0.1.0");
}

#[test]
fn versions_compare_numerically_and_pre_releases_by_their_identifiers() {
    let order = ["0.0.9", "0.0.10", "0.1.0-alpha", "0.1.0-alpha.2", "0.1.0-alpha.10", "0.1.0-beta", "0.1.0", "1.0.0"];

    let parsed: Vec<Version> = order.iter().map(|v| v.parse().unwrap()).collect();

    assert!(parsed.windows(2).all(|w| w[0] < w[1]), "out of order: {parsed:?}");
}

#[test]
fn a_tag_parses_with_or_without_its_v() {
    let tagged: Version = "v0.1.0-rc.1".parse().unwrap();

    assert_eq!(tagged.to_string(), "0.1.0-rc.1");
}

#[test]
fn a_stable_install_is_offered_only_the_newest_newer_stable_release() {
    let tags = ["v0.3.0-rc.1", "v0.2.0", "v0.1.0", "v0.0.1"];

    assert_eq!(offered_tag("0.1.0", &tags).as_deref(), Some("v0.2.0"));
}

#[test]
fn a_stable_install_is_offered_nothing_when_only_pre_releases_are_newer() {
    assert_eq!(offered_tag("0.1.0", &["v0.2.0-rc.1", "v0.1.0"]), None);
}

#[test]
fn a_pre_release_install_is_offered_the_newest_release_stable_or_not() {
    assert_eq!(offered_tag("0.1.0-rc.1", &["v0.1.0", "v0.2.0-rc.1"]).as_deref(), Some("v0.2.0-rc.1"));
    assert_eq!(offered_tag("0.1.0-rc.1", &["v0.1.0-rc.2", "v0.1.0"]).as_deref(), Some("v0.1.0"));
}

#[test]
fn a_current_install_is_offered_nothing() {
    assert_eq!(offered_tag("0.2.0", &["v0.2.0", "v0.1.0"]), None);
}

#[test]
fn releases_are_listed_from_the_github_api_without_drafts_or_unversioned_tags() {
    let mut github = mockito::Server::new();
    let body = serde_json::json!([
        { "tag_name": "v0.2.0", "draft": true, "assets": [] },
        { "tag_name": "nightly", "draft": false, "assets": [] },
        { "tag_name": "v0.1.0", "draft": false, "assets": [
            { "name": "SHA256SUMS", "browser_download_url": "https://example.test/SHA256SUMS" }
        ] }
    ]);
    let list = github
        .mock("GET", "/repos/jkoestinger/akshually/releases")
        .match_query(mockito::Matcher::Any)
        .match_header("user-agent", mockito::Matcher::Regex("^akshually/".into()))
        .with_body(body.to_string())
        .create();

    let releases = Releases::new(&format!("{}/repos/jkoestinger/akshually", github.url())).list().unwrap();

    list.assert();
    assert_eq!(
        releases,
        [Release {
            tag: "v0.1.0".into(),
            version: "0.1.0".parse().unwrap(),
            assets: vec![Asset { name: "SHA256SUMS".into(), url: "https://example.test/SHA256SUMS".into() }],
        }]
    );
}

#[test]
fn a_refused_release_lookup_is_an_error_naming_the_status() {
    let mut github = mockito::Server::new();
    github
        .mock("GET", mockito::Matcher::Any)
        .with_status(403)
        .with_body(r#"{"message":"API rate limit exceeded"}"#)
        .create();

    let error = Releases::new(&github.url()).list().unwrap_err();

    assert!(matches!(error, UpdateError::Api { status: 403, .. }), "{error:?}");
    assert!(error.to_string().contains("API rate limit exceeded"), "{error}");
}

const TAG: &str = "v9.0.0";
const LINUX: &str = "x86_64-unknown-linux-gnu";
const MACOS: &str = "aarch64-apple-darwin";

/// The release `TAG` as the fake GitHub lists it, and the updater of the install `exe` from
/// there, as `target`.
fn release_and_updater(github: &FakeReleases, target: Target, exe: &Path) -> (Release, Updater) {
    let releases = Releases::new(&github.api());
    let release = releases.list().unwrap().into_iter().find(|r| r.tag == TAG).expect("the fake release is listed");
    (release, Updater::new(releases, exe.to_owned()).target(target))
}

/// A Linux release asset, as `packaging/package.sh linux` makes it: `<stem>/akshually` in a
/// `.tar.gz`, the binary holding `binary`.
fn linux_asset(binary: &str) -> (String, Vec<u8>) {
    let stem = format!("akshually-{TAG}-{LINUX}");
    let archive = tar_gz(&stem, &[("akshually", binary), ("akshually.desktop", "[Desktop Entry]\n")]);
    (format!("{stem}.tar.gz"), archive)
}

/// A macOS release asset, as `packaging/package.sh macos` makes it: `akshually.app` in a `.zip`,
/// its binary and `Info.plist` holding `binary`.
fn macos_asset(binary: &str) -> (String, Vec<u8>) {
    let archive = zip("akshually.app", &[("Contents/MacOS/akshually", binary), ("Contents/Info.plist", binary)]);
    (format!("akshually-{TAG}-{MACOS}.zip"), archive)
}

/// Writes an `akshually.app` bundle at `app` whose binary holds `binary`; returns the binary.
fn write_bundle(app: &Path, binary: &str) -> PathBuf {
    let exe = app.join("Contents/MacOS/akshually");
    fs::create_dir_all(exe.parent().unwrap()).unwrap();
    fs::write(app.join("Contents/Info.plist"), binary).unwrap();
    write_executable(&exe, binary);
    exe
}

/// An installed Linux binary holding `contents`, in a dir of its own.
fn linux_install(contents: &str) -> (TestDirs, PathBuf) {
    let dir = TestDirs::new();
    let exe = dir.path().join("akshually");
    write_executable(&exe, contents);
    (dir, exe)
}

#[test]
fn installing_a_release_replaces_a_linux_binary_in_place() {
    let (name, archive) = linux_asset("new binary");
    let github = FakeReleases::new(vec![FakeRelease::new(TAG).asset(&name, archive)]);
    let (dir, exe) = linux_install("old binary");
    let (release, updater) = release_and_updater(&github, Target::new(LINUX, Archive::TarGz), &exe);

    updater.install(&release).unwrap();

    assert_eq!(fs::read_to_string(&exe).unwrap(), "new binary");
    assert_eq!(fs::metadata(&exe).unwrap().permissions().mode() & 0o777, 0o755, "the new binary isn't executable");
    let left: Vec<_> = fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(left, ["akshually"], "the update left files beside the install");
}

#[test]
fn installing_a_release_replaces_a_macos_bundle_as_a_whole() {
    let (name, archive) = macos_asset("new binary");
    let github = FakeReleases::new(vec![FakeRelease::new(TAG).asset(&name, archive)]);
    let apps = TestDirs::new();
    let app = apps.path().join("akshually.app");
    let exe = write_bundle(&app, "old binary");
    fs::write(app.join("Contents/stale"), "only in the old bundle").unwrap();
    let (release, updater) = release_and_updater(&github, Target::new(MACOS, Archive::Zip), &exe);

    updater.install(&release).unwrap();

    assert_eq!(fs::read_to_string(&exe).unwrap(), "new binary");
    assert_eq!(fs::read_to_string(app.join("Contents/Info.plist")).unwrap(), "new binary");
    assert!(!app.join("Contents/stale").exists(), "the old bundle's files outlived the update");
    let left: Vec<_> = fs::read_dir(apps.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(left, ["akshually.app"], "the update left files beside the bundle");
}

#[test]
fn a_hash_mismatch_leaves_the_install_untouched() {
    let (name, archive) = linux_asset("tampered binary");
    let github = FakeReleases::new(vec![FakeRelease::new(TAG).asset(&name, archive).wrong_hash_for(&name)]);
    let (_dir, exe) = linux_install("old binary");
    let (release, updater) = release_and_updater(&github, Target::new(LINUX, Archive::TarGz), &exe);

    let error = updater.install(&release).unwrap_err();

    assert!(matches!(error, UpdateError::HashMismatch { .. }), "{error:?}");
    assert_eq!(fs::read_to_string(&exe).unwrap(), "old binary");
}

#[test]
fn an_unwritable_install_is_refused_with_a_pointer_to_the_install_script() {
    let (name, archive) = linux_asset("new binary");
    let github = FakeReleases::new(vec![FakeRelease::new(TAG).asset(&name, archive)]);
    let (dir, exe) = linux_install("old binary");
    let (release, updater) = release_and_updater(&github, Target::new(LINUX, Archive::TarGz), &exe);
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).unwrap();

    let error = updater.install(&release).unwrap_err();

    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(error, UpdateError::NotWritable { .. }), "{error:?}");
    assert!(error.to_string().contains("install.sh"), "{error}");
    assert_eq!(fs::read_to_string(&exe).unwrap(), "old binary");
}

#[test]
fn a_release_without_this_platforms_asset_is_refused() {
    let (name, archive) = linux_asset("new binary");
    let github = FakeReleases::new(vec![FakeRelease::new(TAG).asset(&name, archive)]);
    let (_dir, exe) = linux_install("old binary");
    let (release, updater) = release_and_updater(&github, Target::new(MACOS, Archive::Zip), &exe);

    let error = updater.install(&release).unwrap_err();

    assert!(matches!(error, UpdateError::NoAsset { .. }), "{error:?}");
    assert_eq!(fs::read_to_string(&exe).unwrap(), "old binary");
}

#[test]
fn a_bundle_that_cannot_be_moved_is_refused_with_a_pointer_to_the_install_script() {
    let (name, archive) = macos_asset("new binary");
    let github = FakeReleases::new(vec![FakeRelease::new(TAG).asset(&name, archive)]);
    let apps = TestDirs::new();
    let app = apps.path().join("akshually.app");
    let exe = write_bundle(&app, "old binary");
    let (release, updater) = release_and_updater(&github, Target::new(MACOS, Archive::Zip), &exe);
    fs::set_permissions(&app, fs::Permissions::from_mode(0o555)).unwrap();

    let error = updater.install(&release).unwrap_err();

    fs::set_permissions(&app, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(error, UpdateError::NotWritable { .. }), "{error:?}");
    assert_eq!(fs::read_to_string(&exe).unwrap(), "old binary");
}
