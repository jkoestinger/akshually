//! UI: a headless render smoke check (Rune's `--snap`), and the Session header's Changeset view
//! dropdown, read from the accessibility tree of a headless render (`--tree`) driven by a
//! `--script`. Needs a Vulkan or Metal adapter, but no display; on a GPU-less Linux CI runner,
//! install lavapipe (`mesa-vulkan-drivers`).

use std::path::Path;
use std::process::Command;

use akshually_testkit::TestDirs;
use serde_json::{Value, json};

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

#[test]
fn a_session_with_a_last_round_shows_its_view_in_one_dropdown() {
    let tree = session_tree(&[]);

    let views = tree.with_role("combo-box");
    assert_eq!(views.len(), 1, "one dropdown in the header: {views:#?}");
    assert_eq!(views[0]["name"], "Changeset view");
    assert_eq!(views[0]["value"], "Since last Round", "the dropdown shows the active view");
    assert!(tree.with_role("button").iter().all(|b| b["name"] != "Full Changeset"), "the view buttons are gone");
}

#[test]
fn opening_the_dropdown_lists_the_views_that_apply_with_the_active_one_checked() {
    let tree = session_tree(&[open_views()]);

    let options = tree.with_role("list-box-option");
    let names: Vec<&Value> = options.iter().map(|o| &o["name"]).collect();
    assert_eq!(names, ["Since last Round", "Since Session base", "Full Changeset"]);
    let checked: Vec<&Value> = options.iter().filter(|o| has_state(o, "selected")).map(|o| &o["name"]).collect();
    assert_eq!(checked, ["Since last Round"], "the active view is checked");
}

#[test]
fn choosing_a_view_closes_the_dropdown() {
    // The scene's service has no Sessions, so the view itself can't change here: choosing goes
    // through `Service::show` as the view buttons did.
    let tree = session_tree(&[open_views(), json!({ "t": 0.6, "type": "click", "key": "option2" })]);

    assert!(tree.with_role("list-box-option").is_empty(), "the dropdown is still open");
}

#[test]
fn escape_closes_the_dropdown_without_changing_the_view() {
    let tree = session_tree(&[open_views(), json!({ "t": 0.6, "type": "key", "name": "Escape" })]);

    assert!(tree.with_role("list-box-option").is_empty(), "the dropdown is still open");
    assert_eq!(tree.with_role("combo-box")[0]["value"], "Since last Round");
}

#[test]
fn clicking_outside_closes_the_dropdown_without_changing_the_view() {
    let tree = session_tree(&[open_views(), json!({ "t": 0.6, "type": "click", "x": 640, "y": 500 })]);

    assert!(tree.with_role("list-box-option").is_empty(), "the dropdown is still open");
    assert_eq!(tree.with_role("combo-box")[0]["value"], "Since last Round");
}

#[test]
fn a_session_where_only_the_full_changeset_applies_shows_no_dropdown() {
    // The second tab is a Round 1 Session without a Session base.
    let tree = session_tree(&[json!({ "t": 0.2, "type": "click", "key": "t-unseen" })]);

    let unseen = tree.with_role("tab").into_iter().find(|t| t["key"] == "t-unseen").unwrap();
    assert!(has_state(unseen, "selected"), "the Round 1 Session's tab didn't open");
    assert!(tree.with_role("combo-box").is_empty(), "a dropdown with a single view");
}

fn render_session(theme: &str) -> Snapshot {
    let dirs = TestDirs::new();
    let snap = dirs.path().join("session.png");

    let out = app(&dirs).arg("--snap").arg(&snap).args(["--theme", theme]).output().unwrap();

    assert!(out.status.success(), "snap failed: {}", String::from_utf8_lossy(&out.stderr));
    assert!(!dirs.runtime().exists() && !dirs.state().exists(), "a headless render touched akshually's dirs");
    Snapshot::read(&snap)
}

/// The accessibility tree of the `session` scene once `script`, a list of Rune `--script`
/// events, has played.
fn session_tree(script: &[Value]) -> Tree {
    let dirs = TestDirs::new();
    let (tree, script_path) = (dirs.path().join("tree.json"), dirs.path().join("script.json"));
    std::fs::write(&script_path, json!(script).to_string()).unwrap();

    let out =
        app(&dirs).arg("--tree").arg(&tree).arg("--script").arg(&script_path).args(["--at", "2"]).output().unwrap();

    assert!(out.status.success(), "tree failed: {}", String::from_utf8_lossy(&out.stderr));
    Tree(serde_json::from_str::<Value>(&std::fs::read_to_string(&tree).unwrap()).unwrap()["accessibility"].clone())
}

/// A headless run of the app on the `session` scene, kept away from akshually's dirs.
fn app(dirs: &TestDirs) -> Command {
    let mut app = Command::new(env!("CARGO_BIN_EXE_akshually"));
    app.args(["app", "--scene", "session", "--size", "1280x800"])
        // A headless render must not bind the socket or write state.
        .env("AKSHUALLY_STATE_DIR", dirs.state())
        .env("AKSHUALLY_RUNTIME_DIR", dirs.runtime())
        // Nor read the user's config.
        .env("AKSHUALLY_CONFIG", dirs.config());
    app
}

/// A press on the Changeset view dropdown.
fn open_views() -> Value {
    json!({ "t": 0.2, "type": "click", "key": "changeset-view-trigger" })
}

fn has_state(node: &Value, state: &str) -> bool {
    node["states"].as_array().is_some_and(|states| states.iter().any(|s| s == state))
}

/// A `--tree` accessibility tree.
struct Tree(Value);

impl Tree {
    /// Every node with `role`, in tree order.
    fn with_role(&self, role: &str) -> Vec<&Value> {
        fn walk<'a>(node: &'a Value, role: &str, found: &mut Vec<&'a Value>) {
            if node["role"] == role {
                found.push(node);
            }
            for child in node["children"].as_array().into_iter().flatten() {
                walk(child, role, found);
            }
        }
        let mut found = Vec::new();
        walk(&self.0, role, &mut found);
        found
    }
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
