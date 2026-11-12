//! akshually's UI: a thin Rune shell over the core review service (ADR 0001).
//!
//! The view model is rebuilt from the service's plain-data Sessions; every human action goes
//! straight back to the service. No review logic lives here.

use std::sync::{Arc, OnceLock};

use akshually_core::{Changeset, ChangesetView, Collapsed, FileChange, Service, SessionId, SessionView};
use rune::{AppSpec, Cx, El, KeyEvent, Tx, col, row, text};
use rune_kit::diff::{DiffState, DiffView, FileDiff};
use rune_kit::shell::{TabItem, TabOrientation, TabStrip};
use rune_kit::{Button, ButtonVariant, Kit};

/// Whether `rune_args` ask Rune for a headless render (`--snap`, `--tree`, ...) rather than
/// a window. Such a run must not bind the socket or touch the state dir.
pub fn is_headless_run(rune_args: &[String]) -> bool {
    rune::tooling::Args::parse_from(rune_args.to_vec()).headless()
}

/// Runs the app's window over `service` until the app quits. `rune_args` are Rune's own
/// flags, without the program name.
pub fn run(service: Arc<Service>, rune_args: Vec<String>) -> Result<(), String> {
    let spec = AppSpec {
        scenes: &[("session", scene_session)],
        families: ("Inter", "ui-monospace, Inter"),
        on_key: Some(on_key),
        ..AppSpec::new("akshually", (1280.0, 800.0), || State::new(Arc::new(Service::new())), view)
    };
    let init_service = service.clone();
    let app = rune::App::new(spec)
        .args(rune_args)
        .init(move || State::new(init_service))
        // The app outlives its window: agents keep calling in (ADR 0002).
        .keep_running();
    let handle = app.handle();
    service.subscribe(move |sessions| {
        let sessions = sessions.to_vec();
        let any = !sessions.is_empty();
        handle.update(move |s: &mut State, _| s.apply(sessions));
        if any && !handle.is_visible() {
            handle.show();
        }
    });
    app.try_run()
}

/// The window's view model.
struct State {
    service: Arc<Service>,
    tabs: Vec<SessionTab>,
    active: Option<SessionId>,
}

struct SessionTab {
    session: SessionView,
    diff: DiffState,
}

impl SessionTab {
    fn new(session: SessionView) -> Self {
        let changeset = session.shown();
        let mut diff = DiffState::new(file_diffs(changeset));
        for (at, file) in changeset.files.iter().enumerate() {
            if file.collapsed.is_some() {
                diff.set_collapsed(at, true);
            }
        }
        SessionTab { session, diff }
    }

    fn title(&self) -> String {
        let repo = self
            .session
            .repo
            .file_name()
            .map_or_else(|| self.session.repo.display().to_string(), |n| n.to_string_lossy().into_owned());
        format!("{repo} · {}", self.session.branch)
    }
}

fn file_diffs(changeset: &Changeset) -> Vec<FileDiff> {
    changeset
        .files
        .iter()
        .map(|f| {
            let path = |side: &Option<String>| side.as_ref().map(|_| f.path.clone());
            let mut diff = FileDiff::between(
                path(&f.old),
                path(&f.new),
                f.old.as_deref().unwrap_or(""),
                f.new.as_deref().unwrap_or(""),
            );
            diff.binary = f.binary.is_some();
            diff
        })
        .collect()
}

/// What a file's header says about it beyond the diff: "binary changed (size)", or why it is
/// collapsed.
fn file_note(file: &FileChange) -> Option<String> {
    if let Some(sizes) = file.binary {
        let size = match (sizes.old, sizes.new) {
            (Some(old), Some(new)) => format!("{} → {}", bytes(old), bytes(new)),
            (old, new) => bytes(old.or(new).unwrap_or(0)),
        };
        return Some(format!("binary changed ({size})"));
    }
    file.collapsed.map(|why| match why {
        Collapsed::Generated => "Generated file".to_owned(),
        Collapsed::Large => "Large file".to_owned(),
    })
}

fn bytes(n: u64) -> String {
    match n {
        0..1024 => format!("{n} B"),
        1024..1_048_576 => format!("{:.1} KB", n as f64 / 1024.0),
        _ => format!("{:.1} MB", n as f64 / 1_048_576.0),
    }
}

impl State {
    fn new(service: Arc<Service>) -> Self {
        let mut state = State { service, tabs: Vec::new(), active: None };
        let sessions = state.service.sessions();
        state.apply(sessions);
        state
    }

    /// Mirrors the service's Sessions, keeping the view state of Sessions that didn't change.
    fn apply(&mut self, sessions: Vec<SessionView>) {
        let mut old = std::mem::take(&mut self.tabs);
        self.tabs = sessions
            .into_iter()
            .map(|session| match old.iter().position(|t| t.session == session) {
                Some(at) => old.swap_remove(at),
                None => SessionTab::new(session),
            })
            .collect();
        if self.active_tab().is_none() {
            self.active = self.tabs.first().map(|t| t.session.id.clone());
        }
    }

    fn show(&self, id: &SessionId, view: ChangesetView) {
        if let Err(error) = self.service.show(id, view) {
            eprintln!("akshually: {error}");
        }
    }

    fn active_tab(&self) -> Option<&SessionTab> {
        self.tabs.iter().find(|t| Some(&t.session.id) == self.active.as_ref())
    }

    fn active_tab_mut(&mut self) -> Option<&mut SessionTab> {
        self.tabs.iter_mut().find(|t| Some(&t.session.id) == self.active.as_ref())
    }

    /// Runs a human action on the service; the service's update then refreshes the view.
    fn act(&self, id: &SessionId, action: fn(&Service, &SessionId) -> Result<(), akshually_core::Error>) {
        if let Err(error) = action(&self.service, id) {
            eprintln!("akshually: {error}");
        }
    }
}

fn kit(dark: bool) -> &'static Kit {
    static DARK: OnceLock<Kit> = OnceLock::new();
    static LIGHT: OnceLock<Kit> = OnceLock::new();
    let cell = if dark { &DARK } else { &LIGHT };
    cell.get_or_init(|| Kit::neutral(dark))
}

fn view(s: &State, cx: &mut Cx<State>) -> El {
    let k = kit(cx.system_dark());
    let mut layer = Vec::new();
    let body = match s.active_tab() {
        Some(tab) => {
            let tabs = TabStrip::new("tabs", TabOrientation::Horizontal)
                .tabs(s.tabs.iter().map(|t| {
                    TabItem::new(t.session.id.as_str(), t.title()).icon(&rune_kit::glyphs::FILE_CODE).closable(false)
                }))
                .active(s.active.as_ref().map(SessionId::as_str))
                .viewport_width(cx.layout_size("tabbar").map(|z| z[0]))
                .on_select(|s: &mut State, id: &str, _| {
                    s.active = s.tabs.iter().find(|t| t.session.id.as_str() == id).map(|t| t.session.id.clone());
                })
                .build_layered(k, &mut layer);
            col()
                .w_full()
                .h_full()
                .min_h(0.0)
                .child(row().key("tabbar").h(40.0).w_full().child(tabs))
                .child(session_header(k, tab))
                .child(col().grow(1.0).min_h(0.0).child(session_diff(k, s, tab)))
        }
        None => col().w_full().h_full().items_center().justify_center().child(
            text("No Sessions. Run `akshually review` in a repository to ask for a review.")
                .font(k.ty.ui)
                .color(k.c.text_muted),
        ),
    };
    rune_kit::root(body.fill(k.c.bg_base), layer)
}

fn session_header(k: &Kit, tab: &SessionTab) -> El {
    let session = &tab.session;
    let shown = session.shown();
    let base = if shown == &session.changeset {
        format!("→ {}", &shown.base[..shown.base.len().min(8)])
    } else {
        "since Session base".to_owned()
    };
    let (approve_id, abandon_id) = (session.id.clone(), session.id.clone());
    let toggle = session.since_session_base.as_ref().map(|_| {
        let view_button = |label: &str, view: ChangesetView| {
            let id = session.id.clone();
            let variant = if session.view == view { ButtonVariant::Neutral } else { ButtonVariant::Ghost };
            Button::new(label).variant(variant).on_press(move |s: &mut State| s.show(&id, view)).build(k)
        };
        row()
            .gap(4.0)
            .child(view_button("Since Session base", ChangesetView::SinceSessionBase))
            .child(view_button("Full Changeset", ChangesetView::Full))
    });
    row()
        .key("session-header")
        .w_full()
        .h(56.0)
        .px(16.0)
        .gap(12.0)
        .items_center()
        .fill(k.c.layer_01)
        .child(text(session.branch.clone()).font(k.ty.mono).color(k.c.text_base))
        .child(text(base).font(k.ty.mono).color(k.c.text_muted))
        .child(row().grow(1.0))
        .children(toggle)
        .child(
            Button::new("Abandon")
                .variant(ButtonVariant::Ghost)
                .on_press(move |s: &mut State| s.act(&abandon_id, Service::abandon))
                .build(k),
        )
        .child(
            Button::new("Approve")
                .variant(ButtonVariant::Contrast)
                .on_press(move |s: &mut State| s.act(&approve_id, Service::approve))
                .build(k),
        )
}

fn session_diff(k: &Kit, s: &State, tab: &SessionTab) -> El {
    let id = tab.session.id.clone();
    let id_mut = id.clone();
    let id_header = id.clone();
    let kc = k.clone();
    DiffView::new(
        move |s: &State| &s.tabs.iter().find(|t| t.session.id == id).expect("the active tab").diff,
        move |s: &mut State| &mut s.tabs.iter_mut().find(|t| t.session.id == id_mut).expect("the active tab").diff,
    )
    .key(format!("diff-{}", tab.session.id))
    .header_actions(move |s: &State, file: usize| {
        let tab = s.tabs.iter().find(|t| t.session.id == id_header).expect("the active tab");
        let note = tab.session.shown().files.get(file).and_then(file_note).unwrap_or_default();
        text(note).font(kc.ty.small).color(kc.c.text_muted)
    })
    .build(s, k)
}

fn on_key(s: &mut State, ev: &KeyEvent, tx: &mut Tx) -> bool {
    s.active_tab_mut().is_some_and(|t| t.diff.key(ev, tx))
}

/// `--scene session`: a sample Session, for headless renders.
fn scene_session(s: &mut State) {
    let changeset: Changeset = sample_changeset();
    let session = SessionView {
        id: SessionId::new("sample"),
        repo: "/home/me/akshually".into(),
        branch: "feat/round-snapshots".into(),
        since_session_base: Some(Changeset { base: "5e5510nba5e".into(), files: changeset.files[..2].to_vec() }),
        changeset,
        view: ChangesetView::Full,
    };
    s.apply(vec![session]);
}

fn sample_changeset() -> Changeset {
    Changeset {
        base: "3e61ed7c0ffee".into(),
        files: vec![
            FileChange {
                binary: None,
                collapsed: None,
                path: "src/session.rs".into(),
                old: Some("pub fn hand_over_round(&mut self) -> Result<Round> {\n    let drafts = self.take_drafts();\n    let round = Round::new(drafts);\n    Ok(round)\n}\n".into()),
                new: Some("pub fn hand_over_round(&mut self) -> Result<Round> {\n    let drafts = self.take_drafts();\n    let snapshot = self.changeset.snapshot()?;\n    let round = Round::new(drafts, snapshot);\n    Ok(round)\n}\n".into()),
            },
            FileChange::text("src/round.rs", None, Some("pub struct Round;\n")),
            FileChange {
                binary: Some(akshually_core::BinarySizes { old: Some(12_288), new: Some(14_745) }),
                ..FileChange::text("docs/screenshot.png", Some(""), Some(""))
            },
            FileChange {
                collapsed: Some(Collapsed::Generated),
                ..FileChange::text("Cargo.lock", Some("# @generated\nold\n"), Some("# @generated\nnew\n"))
            },
        ],
    }
}
