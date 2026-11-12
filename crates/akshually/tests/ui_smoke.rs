//! UI: a headless render smoke check only (Rune's `--snap`). Needs a Vulkan or Metal adapter,
//! but no display; on a GPU-less Linux CI runner, install lavapipe (`mesa-vulkan-drivers`).

use std::process::Command;

use akshually_testkit::TestDirs;

#[test]
fn the_app_renders_a_session_headlessly() {
    let dirs = TestDirs::new();
    let snap = dirs.path().join("session.png");

    let out = Command::new(env!("CARGO_BIN_EXE_akshually"))
        .args(["app", "--snap"])
        .arg(&snap)
        .args(["--scene", "session", "--theme", "dark", "--size", "1280x800"])
        // A headless render must not bind the socket or write state.
        .env("AKSHUALLY_STATE_DIR", dirs.state())
        .env("AKSHUALLY_RUNTIME_DIR", dirs.runtime())
        .output()
        .unwrap();

    assert!(out.status.success(), "snap failed: {}", String::from_utf8_lossy(&out.stderr));
    assert!(std::fs::metadata(&snap).unwrap().len() > 0);
    assert!(!dirs.runtime().exists() && !dirs.state().exists(), "a headless render touched akshually's dirs");
}
