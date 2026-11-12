//! akshually's UI: a thin Rune shell over the core review service (ADR 0001).
//!
//! The view model is rebuilt from the service's plain-data Sessions; every human action goes
//! straight back to the service. No review logic lives here.

use std::sync::Arc;

use akshually_core::{Changeset, Service, SessionId, SessionView};
use rune::{AppSpec, Cx, El, KeyEvent, Tx, col, row, text};
use rune_kit::diff::{DiffState, DiffView, FileDiff};
use rune_kit::shell::{TabItem, TabOrientation, TabStrip};
use rune_kit::{Button, ButtonVariant, Kit};

mod theme;

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
        // The system monospace font. A family whose oblique face lacks the italic/oblique bit
        // in its OS/2 table (some Berkeley Mono builds) renders slanted: the font's metadata,
        // not ours; fontconfig goes by the italic angle, Rune's font matching doesn't.
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
        let diff = DiffState::new(file_diffs(&session.changeset));
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
            FileDiff::between(
                path(&f.old),
                path(&f.new),
                f.old.as_deref().unwrap_or(""),
                f.new.as_deref().unwrap_or(""),
            )
        })
        .collect()
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

fn view(s: &State, cx: &mut Cx<State>) -> El {
    // Rune keeps `system_dark` current with the OS setting and redraws when it changes.
    let k = theme::kit(cx.system_dark());
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
    let base = &session.changeset.base[..session.changeset.base.len().min(8)];
    let (approve_id, abandon_id) = (session.id.clone(), session.id.clone());
    row()
        .key("session-header")
        .w_full()
        .h(56.0)
        .px(16.0)
        .gap(12.0)
        .items_center()
        .fill(k.c.layer_01)
        .child(text(session.branch.clone()).font(k.ty.mono).color(k.c.text_base))
        .child(text(format!("→ {base}")).font(k.ty.mono).color(k.c.text_muted))
        .child(row().grow(1.0))
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
    DiffView::new(
        move |s: &State| &s.tabs.iter().find(|t| t.session.id == id).expect("the active tab").diff,
        move |s: &mut State| &mut s.tabs.iter_mut().find(|t| t.session.id == id_mut).expect("the active tab").diff,
    )
    .key(format!("diff-{}", tab.session.id))
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
        changeset,
    };
    s.apply(vec![session]);
}

fn sample_changeset() -> Changeset {
    use akshually_core::FileChange;
    Changeset {
        base: "3e61ed7c0ffee".into(),
        files: vec![
            FileChange {
                path: "src/session.rs".into(),
                old: Some("pub fn hand_over_round(&mut self) -> Result<Round> {\n    let drafts = self.take_drafts();\n    let round = Round::new(drafts);\n    Ok(round)\n}\n".into()),
                new: Some("pub fn hand_over_round(&mut self) -> Result<Round> {\n    let drafts = self.take_drafts();\n    let snapshot = self.changeset.snapshot()?;\n    let round = Round::new(drafts, snapshot);\n    Ok(round)\n}\n".into()),
            },
            FileChange { path: "src/round.rs".into(), old: None, new: Some("pub struct Round;\n".into()) },
        ],
    }
}
