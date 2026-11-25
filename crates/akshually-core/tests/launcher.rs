//! Seam A: the launch repair of the Linux launcher entry and icon, against a throwaway data
//! home, with the files `packaging/` ships as the expected content.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use akshually_core::repair_launcher;
use akshually_testkit::TestDirs;

const SIZES: [&str; 7] = ["16x16", "32x32", "48x48", "64x64", "128x128", "256x256", "512x512"];

fn packaging() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging")
}

fn entry(data: &Path) -> PathBuf {
    data.join("applications/akshually.desktop")
}

fn icon(data: &Path, size: &str) -> PathBuf {
    data.join(format!("icons/hicolor/{size}/apps/akshually.png"))
}

/// A data home holding an older launcher entry and no icon, as an install from before the icon has.
fn old_install(data: &Path) {
    fs::create_dir_all(data.join("applications")).unwrap();
    fs::write(entry(data), "[Desktop Entry]\nName=akshually\n").unwrap();
}

#[test]
fn an_older_launcher_entry_is_replaced_and_the_icon_installed() {
    let dirs = TestDirs::new();
    old_install(dirs.path());

    repair_launcher(dirs.path()).unwrap();

    let current = fs::read(packaging().join("linux/akshually.desktop")).unwrap();
    assert_eq!(fs::read(entry(dirs.path())).unwrap(), current, "the launcher entry isn't the current one");
    for size in SIZES {
        let shipped = fs::read(packaging().join(format!("icons/hicolor/{size}/apps/akshually.png"))).unwrap();
        assert_eq!(fs::read(icon(dirs.path(), size)).unwrap(), shipped, "the {size} icon isn't the shipped one");
    }
}

#[test]
fn nothing_is_written_when_there_is_no_launcher_entry() {
    let dirs = TestDirs::new();

    let written = repair_launcher(dirs.path()).unwrap();

    assert!(written.is_empty(), "wrote {written:?}");
    assert_eq!(fs::read_dir(dirs.path()).unwrap().count(), 0, "left files in the data home");
}

#[test]
fn a_current_install_is_left_alone() {
    let dirs = TestDirs::new();
    old_install(dirs.path());
    repair_launcher(dirs.path()).unwrap();

    let written = repair_launcher(dirs.path()).unwrap();

    assert!(written.is_empty(), "rewrote {written:?}");
}

#[test]
fn a_changed_icon_is_restored() {
    let dirs = TestDirs::new();
    old_install(dirs.path());
    repair_launcher(dirs.path()).unwrap();
    fs::write(icon(dirs.path(), "48x48"), "not an icon").unwrap();

    repair_launcher(dirs.path()).unwrap();

    let shipped = fs::read(packaging().join("icons/hicolor/48x48/apps/akshually.png")).unwrap();
    assert_eq!(fs::read(icon(dirs.path(), "48x48")).unwrap(), shipped);
}

#[test]
fn an_unwritable_data_home_is_an_error_not_a_panic() {
    let dirs = TestDirs::new();
    old_install(dirs.path());
    let locked = [dirs.path().to_owned(), dirs.path().join("applications")];
    for dir in &locked {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o555)).unwrap();
    }

    let result = repair_launcher(dirs.path());

    for dir in &locked {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
    }
    assert!(result.is_err(), "reported success writing to a read-only data home");
}
