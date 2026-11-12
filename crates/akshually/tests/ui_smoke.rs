//! UI: a headless render smoke check only (Rune's `--snap`). Needs a Vulkan or Metal adapter,
//! but no display; on a GPU-less Linux CI runner, install lavapipe (`mesa-vulkan-drivers`).

use std::path::Path;
use std::process::Command;

use akshually_testkit::TestDirs;

/// The window background and the Session header's surface, in logical pixels of a 1280x800
/// render: below the last file of the sample Session, and an empty stretch of its header.
const BG_AT: (u32, u32) = (1270, 790);
const SURFACE_AT: (u32, u32) = (800, 60);

#[test]
fn the_app_renders_a_session_in_the_dark_theme() {
    // `bg` and `surface` of the Dark mode of the `akshually` variables in design/akshually.figcraft.
    let snap = render_session("dark");
    assert_eq!(snap.pixel(BG_AT), [0x16, 0x18, 0x1d], "bg");
    assert_eq!(snap.pixel(SURFACE_AT), [0x1e, 0x21, 0x28], "surface");
}

#[test]
fn the_app_renders_a_session_in_the_light_theme() {
    // `bg` and `surface` of the Light mode of the `akshually` variables in design/akshually.figcraft.
    let snap = render_session("light");
    assert_eq!(snap.pixel(BG_AT), [0xff, 0xff, 0xff], "bg");
    assert_eq!(snap.pixel(SURFACE_AT), [0xf6, 0xf7, 0xf9], "surface");
}

fn render_session(theme: &str) -> Snapshot {
    let dirs = TestDirs::new();
    let snap = dirs.path().join("session.png");

    let out = Command::new(env!("CARGO_BIN_EXE_akshually"))
        .args(["app", "--snap"])
        .arg(&snap)
        .args(["--scene", "session", "--theme", theme, "--size", "1280x800"])
        // A headless render must not bind the socket or write state.
        .env("AKSHUALLY_STATE_DIR", dirs.state())
        .env("AKSHUALLY_RUNTIME_DIR", dirs.runtime())
        // Nor read the user's config.
        .env("AKSHUALLY_CONFIG", dirs.config())
        .output()
        .unwrap();

    assert!(out.status.success(), "snap failed: {}", String::from_utf8_lossy(&out.stderr));
    assert!(!dirs.runtime().exists() && !dirs.state().exists(), "a headless render touched akshually's dirs");
    Snapshot::read(&snap)
}

/// A decoded `--snap` PNG.
struct Snapshot {
    rgba: Vec<u8>,
    width: u32,
    /// Device pixels per logical pixel.
    scale: u32,
}

impl Snapshot {
    fn read(path: &Path) -> Snapshot {
        let mut decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()));
        decoder.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = decoder.read_info().unwrap();
        let mut rgba = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut rgba).unwrap();
        assert_eq!(info.color_type, png::ColorType::Rgba, "snapshots are RGBA");
        Snapshot { rgba, width: info.width, scale: info.width / 1280 }
    }

    /// The RGB colour at logical pixel `(x, y)`.
    fn pixel(&self, (x, y): (u32, u32)) -> [u8; 3] {
        let at = ((y * self.scale * self.width + x * self.scale) * 4) as usize;
        [self.rgba[at], self.rgba[at + 1], self.rgba[at + 2]]
    }
}
