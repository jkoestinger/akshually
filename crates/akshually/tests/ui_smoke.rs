//! UI: a headless render smoke check (Rune's `--snap`, and `--tree` for the text a scene shows),
//! the Publish dialog, the update notice, and the Session header's Changeset view dropdown and
//! diff layout control, read from the `--tree` accessibility tree after a `--script` of input.
//! Needs a Vulkan or Metal adapter, but no display; on a GPU-less Linux CI runner, install
//! lavapipe (`mesa-vulkan-drivers`).

use std::path::Path;
use std::process::Command;

use akshually_core::DiffLayout;
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

#[test]
fn a_session_shows_its_diff_stacked_by_default() {
    let tree = session_tree(&[]);

    assert_eq!(tree.diff_layout(), DiffLayout::Stacked);
}

#[test]
fn picking_side_by_side_lays_out_every_tab_side_by_side() {
    let tree = session_tree(&[pick_side_by_side(), json!({ "t": 0.4, "type": "click", "key": "t-unseen" })]);

    let unseen = tree.with_role("tab").into_iter().find(|t| t["key"] == "t-unseen").unwrap();
    assert!(has_state(unseen, "selected"), "the other Session's tab didn't open");
    assert_eq!(tree.diff_layout(), DiffLayout::SideBySide);
}

#[test]
fn threads_still_show_under_their_lines_side_by_side() {
    let tree = session_tree(&[pick_side_by_side()]);

    assert_eq!(tree.diff_layout(), DiffLayout::SideBySide);
    assert!(tree.0.to_string().contains("Take the snapshot before building the Round."), "T1 is gone");
}

#[test]
fn the_publish_dialog_names_a_gitlab_pull_request_with_its_sigil() {
    let tree = render_tree("publish-gitlab");

    assert!(
        tree.contains("Sent to gitlab.example.com as one review on platform/infra !97, under your name."),
        "the Publish dialog doesn't name the GitLab Pull Request as `platform/infra !97`"
    );
    assert!(!tree.contains("#97"), "the Publish dialog names a GitLab Pull Request with `#`");
}

#[test]
fn the_publish_dialog_names_a_github_pull_request_with_its_sigil() {
    let tree = render_tree("publish");

    assert!(
        tree.contains("Sent to github.com as one review on acme/web #482, under your name."),
        "the Publish dialog doesn't name the GitHub Pull Request as `acme/web #482`"
    );
}

#[test]
fn the_publish_dialog_shows_a_long_draft_in_full_inside_the_dialog() {
    let tree = scene_tree("publish", &[]);

    let text = tree.0.to_string();
    assert!(text.contains(LONG_DRAFT_FIRST_PARAGRAPH), "the long Draft's first paragraph isn't shown in full");
    assert!(text.contains(LONG_DRAFT_SECOND_PARAGRAPH), "only the long Draft's first line is shown");
    let dialog = &tree.with_role("dialog")[0];
    let inside = Tree((*dialog).clone());
    let wider: Vec<&Value> =
        inside.with_role("label").into_iter().filter(|l| edges(l).right > edges(dialog).right).collect();
    assert!(wider.is_empty(), "text runs past the dialog's right edge: {wider:#?}");
}

#[test]
fn each_publish_card_shows_whether_it_goes_out() {
    let tree = scene_tree("publish", &[]);

    let boxes = tree.with_role("check-box");
    let states: Vec<bool> = boxes.iter().map(|b| has_state(b, "checked")).collect();
    assert_eq!(
        states,
        [true, true, true, true, false, true, true],
        "kept, resolve and reopen are ticked, discarded isn't"
    );
    assert!(boxes.iter().all(|b| has_state(b, "disabled")), "an indicator can be toggled");
    assert!(tree.0.to_string().contains("(not sent)"), "the discarded Draft doesn't say it's not sent");
}

#[test]
fn a_publish_card_shows_its_drafts_suggestion() {
    let tree = scene_tree("publish", &[]);

    let text = tree.0.to_string();
    assert!(text.contains("\"suggestion\""), "the suggestion tag is gone");
    assert!(text.contains("let before = self.changeset.snapshot()?;"), "the suggested code isn't shown");
}

#[test]
fn a_long_publish_list_scrolls_and_keeps_the_verdict_and_buttons_in_a_short_window() {
    let tree = scene_tree_at("publish-many", SHORT_WINDOW, &[]);

    let publish = tree.with_role("button").into_iter().find(|b| b["name"].as_str().unwrap().starts_with("Publish"));
    let publish = publish.expect("no Publish button");
    assert!(edges(publish).bottom <= SHORT_WINDOW.1, "the Publish button is below the window: {publish:#?}");
    let verdicts = tree.with_role("radio-button");
    let below = verdicts.iter().any(|v| edges(v).bottom > SHORT_WINDOW.1);
    assert!(!verdicts.is_empty() && !below, "a Verdict is below the window");
    let lists = tree.with_role("scroll-view").into_iter().filter(|s| s["key"] == "publish-scroll").count();
    assert_eq!(lists, 1, "the Drafts aren't in a scroll area");
}

#[test]
fn the_update_notice_names_the_newer_release_and_offers_update() {
    let tree = scene_tree("update", &[]);

    assert!(tree.0.to_string().contains("akshually 99.0.0 is available."), "no update notice naming the release");
    assert!(tree.button("Update").is_some(), "the notice offers no Update");
}

#[test]
fn dismissing_the_update_notice_hides_it() {
    let tree = scene_tree("update", &[json!({ "t": 0.2, "type": "click", "key": "update-dismiss" })]);

    assert!(!tree.0.to_string().contains("is available"), "the dismissed notice still shows");
}

#[test]
fn an_installed_update_offers_restart() {
    let tree = scene_tree("update-installed", &[]);

    assert!(tree.0.to_string().contains("akshually 99.0.0 is installed."), "the notice doesn't say it's installed");
    assert!(tree.button("Restart").is_some(), "the notice offers no Restart");
}

#[test]
fn restart_with_an_agent_waiting_asks_first_naming_its_session() {
    let tree = scene_tree("update-installed", &[json!({ "t": 0.2, "type": "click", "key": "update-restart" })]);

    let text = tree.0.to_string();
    assert!(text.contains("akshually · feat/round-snapshots"), "the confirmation doesn't name the Session: {text}");
    assert!(text.contains("run `akshually review` again"), "the confirmation doesn't say what the agents must do");
    assert!(tree.button("Restart now").is_some(), "no button to confirm the restart");
}

fn render_session(theme: &str) -> Snapshot {
    let args = ["--scene", "session", "--theme", theme, "--size", "1280x800"];
    render("session.png", "--snap", &args, Snapshot::read)
}

/// The `--tree` dump (Rune's element and accessibility tree, as JSON) of `scene`.
fn render_tree(scene: &str) -> String {
    render("tree.json", "--tree", &["--scene", scene], |path| std::fs::read_to_string(path).unwrap())
}

/// Runs the app headless, writing its `output` flag's file to `name` in a scratch directory, and
/// reads that file with `read` before the directory goes.
fn render<T>(name: &str, output: &str, args: &[&str], read: impl FnOnce(&Path) -> T) -> T {
    let dirs = TestDirs::new();
    let path = dirs.path().join(name);

    let out = Command::new(env!("CARGO_BIN_EXE_akshually"))
        .args(["app", output])
        .arg(&path)
        .args(args)
        // A headless render must not bind the socket or write state.
        .env("AKSHUALLY_STATE_DIR", dirs.state())
        .env("AKSHUALLY_RUNTIME_DIR", dirs.runtime())
        // Nor read the user's config.
        .env("AKSHUALLY_CONFIG", dirs.config())
        .output()
        .unwrap();

    assert!(out.status.success(), "render failed: {}", String::from_utf8_lossy(&out.stderr));
    assert!(!dirs.runtime().exists() && !dirs.state().exists(), "a headless render touched akshually's dirs");
    read(&path)
}

/// The accessibility tree of the `session` scene once `script`, a list of Rune `--script`
/// events, has played.
fn session_tree(script: &[Value]) -> Tree {
    scene_tree("session", script)
}

/// The accessibility tree of `scene` once `script`, a list of Rune `--script` events, has played.
fn scene_tree(scene: &str, script: &[Value]) -> Tree {
    scene_tree_at(scene, (1280.0, 800.0), script)
}

/// [`scene_tree`] in a window of `(width, height)` logical pixels.
fn scene_tree_at(scene: &str, (width, height): (f64, f64), script: &[Value]) -> Tree {
    let dirs = TestDirs::new();
    let script_path = dirs.path().join("script.json");
    std::fs::write(&script_path, json!(script).to_string()).unwrap();

    let size = format!("{width}x{height}");
    let args = ["--scene", scene, "--size", &size, "--script", script_path.to_str().unwrap(), "--at", "2"];
    let tree = render("tree.json", "--tree", &args, |path| std::fs::read_to_string(path).unwrap());

    Tree(serde_json::from_str::<Value>(&tree).unwrap()["accessibility"].clone())
}

/// A window shorter than the default 800 pixels, in logical pixels: the Publish dialog's list must
/// shrink to keep the dialog inside it.
const SHORT_WINDOW: (f64, f64) = (1280.0, 500.0);

/// The long Draft of the `publish` scene, one paragraph at a time.
const LONG_DRAFT_FIRST_PARAGRAPH: &str = "The snapshot is taken after the Round is built, so a Draft the agent adds while \
    the Round is being built lands in neither the old Round nor the new one, and the human never sees it.";
const LONG_DRAFT_SECOND_PARAGRAPH: &str = "Take the snapshot first, then build the Round from it.";

/// A press on the Changeset view dropdown.
fn open_views() -> Value {
    json!({ "t": 0.2, "type": "click", "key": "changeset-view-trigger" })
}

/// A press on the diff layout control's side-by-side segment.
fn pick_side_by_side() -> Value {
    json!({ "t": 0.2, "type": "click", "key": "segment1" })
}

/// The right and bottom edges of `node`'s bounds, in logical pixels.
fn edges(node: &Value) -> Edges {
    let at = |i: usize| node["bounds"][i].as_f64().unwrap();
    Edges { right: at(0) + at(2), bottom: at(1) + at(3) }
}

struct Edges {
    right: f64,
    bottom: f64,
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

    /// The button named `name`, if the tree has one.
    fn button(&self, name: &str) -> Option<&Value> {
        self.with_role("button").into_iter().find(|b| b["name"] == name)
    }

    /// How the shown diff is laid out: side by side when a removed line sits level with an
    /// added one, which stacked never puts on one row.
    fn diff_layout(&self) -> DiffLayout {
        let tops = |kind: &str| -> Vec<f64> {
            let rows = self.with_role("list-item").into_iter();
            let rows = rows.filter(|r| r["name"].as_str().is_some_and(|n| n.starts_with(kind)));
            rows.map(|r| r["bounds"][1].as_f64().unwrap()).collect()
        };
        let (removed, added) = (tops("removed line"), tops("added line"));
        assert!(!removed.is_empty() && !added.is_empty(), "the diff shows no changed lines");
        if removed.iter().any(|top| added.contains(top)) { DiffLayout::SideBySide } else { DiffLayout::Stacked }
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
