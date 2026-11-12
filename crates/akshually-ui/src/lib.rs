//! akshually's UI: a thin Rune shell over the core review service (ADR 0001).
//!
//! The view model is rebuilt from the service's plain-data Sessions; every human action goes
//! straight back to the service. No review logic lives here.

use std::sync::Arc;

use akshually_core::{Anchor, Changeset, Party, Service, SessionId, SessionView, Side, Thread};
use rune::{AppHandle, AppSpec, Cx, El, KeyEvent, TextEdit, Tx, col, row, text};
use rune_kit::diff::{self, Annotation, DiffState, DiffView, FileDiff, LineRange, LineRef};
use rune_kit::shell::{TabItem, TabOrientation, TabStrip};
use rune_kit::{Badge, Button, ButtonVariant, Kit, TextArea, TextInput};

/// The annotation id of the Comment composer (Thread ids are `T1`, `T2`, …).
const COMPOSER: &str = "composer";

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
    let own = handle.clone();
    handle.update(move |s: &mut State, _| s.handle = Some(own));
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
    /// The Comment being written, at most one in the window.
    composer: Option<Composer>,
    composer_text: TextEdit,
    /// The "Open a Pull Request by URL" field, and what became of the last URL opened.
    pr_url: TextEdit,
    pr_status: Option<String>,
    /// For work off the UI thread to report back; `None` in headless renders.
    handle: Option<AppHandle<State>>,
}

/// A Comment being written on a line range, before it becomes a Draft.
struct Composer {
    session: SessionId,
    anchor: Anchor,
    /// Where the composer shows in the diff: the range's last line.
    at: Annotation,
}

struct SessionTab {
    session: SessionView,
    diff: DiffState,
}

impl SessionTab {
    fn new(session: SessionView) -> Self {
        let diff = DiffState::new(file_diffs(&session.changeset));
        let mut tab = SessionTab { session, diff };
        tab.annotate(None);
        tab
    }

    /// Puts each Thread, and the composer when it's on this Session, under its last line.
    fn annotate(&mut self, composer: Option<&Composer>) {
        let changeset = &self.session.changeset;
        let mut notes: Vec<Annotation> = self
            .session
            .threads
            .iter()
            .filter_map(|t| {
                let Anchor::Lines { path, side, end, .. } = &t.anchor;
                let file = changeset.files.iter().position(|f| &f.path == path)?;
                Some(Annotation::new(file, LineRef::new(diff_side(*side), *end), t.id.as_str()))
            })
            .collect();
        notes.extend(composer.filter(|c| c.session == self.session.id).map(|c| c.at.clone()));
        self.diff.set_annotations(notes);
    }

    fn drafts(&self) -> usize {
        self.session.threads.iter().flat_map(|t| &t.comments).filter(|c| c.is_draft()).count()
    }

    fn title(&self) -> String {
        if let Some(pr) = &self.session.pull_request {
            return format!("{} #{}", pr.id.repo, pr.id.number);
        }
        let repo = self
            .session
            .repo
            .file_name()
            .map_or_else(|| self.session.repo.display().to_string(), |n| n.to_string_lossy().into_owned());
        format!("{repo} · {}", self.session.branch)
    }
}

fn diff_side(side: Side) -> diff::Side {
    match side {
        Side::Old => diff::Side::Old,
        Side::New => diff::Side::New,
    }
}

/// The Anchor for lines selected in the diff. A selection may run across sides; it then
/// anchors to its last line alone.
fn anchor_for(changeset: &Changeset, range: LineRange) -> Option<Anchor> {
    let path = changeset.files.get(range.file)?.path.clone();
    let side = match range.end.side {
        diff::Side::Old => Side::Old,
        diff::Side::New => Side::New,
    };
    let (start, end) = if range.start.side == range.end.side {
        (range.start.line.min(range.end.line), range.start.line.max(range.end.line))
    } else {
        (range.end.line, range.end.line)
    };
    Some(Anchor::lines(side, path, start..=end))
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
        let mut state = State {
            service,
            tabs: Vec::new(),
            active: None,
            composer: None,
            composer_text: TextEdit::new(""),
            pr_url: TextEdit::new(""),
            pr_status: None,
            handle: None,
        };
        let sessions = state.service.sessions();
        state.apply(sessions);
        state
    }

    /// Mirrors the service's Sessions, keeping each Session's view state (scroll, folds) and
    /// rebuilding its diff only when its Changeset changed.
    fn apply(&mut self, sessions: Vec<SessionView>) {
        let mut old = std::mem::take(&mut self.tabs);
        self.tabs = sessions
            .into_iter()
            .map(|session| match old.iter().position(|t| t.session.id == session.id) {
                Some(at) => {
                    let mut tab = old.swap_remove(at);
                    if tab.session.changeset != session.changeset {
                        tab.diff.set_files(file_diffs(&session.changeset));
                        // The lines it was on may be gone.
                        if self.composer.as_ref().is_some_and(|c| c.session == session.id) {
                            self.composer = None;
                        }
                    }
                    tab.session = session;
                    tab
                }
                None => SessionTab::new(session),
            })
            .collect();
        if self.composer.as_ref().is_some_and(|c| !self.tabs.iter().any(|t| t.session.id == c.session)) {
            self.composer = None;
        }
        self.annotate();
        if self.active_tab().is_none() {
            self.active = self.tabs.first().map(|t| t.session.id.clone());
        }
    }

    fn annotate(&mut self) {
        for tab in &mut self.tabs {
            tab.annotate(self.composer.as_ref());
        }
    }

    /// Opens the composer on `range` of the active Session.
    fn compose(&mut self, range: LineRange) {
        let Some(tab) = self.active_tab() else { return };
        let Some(anchor) = anchor_for(&tab.session.changeset, range) else { return };
        let at = Annotation::new(range.file, range.end, COMPOSER);
        self.composer = Some(Composer { session: tab.session.id.clone(), anchor, at });
        self.composer_text = TextEdit::new("");
        self.annotate();
    }

    fn close_composer(&mut self) {
        self.composer = None;
        self.composer_text = TextEdit::new("");
        self.annotate();
    }

    /// Turns the composer's text into a Draft in a new Thread.
    fn save_draft(&mut self) {
        let Some(composer) = &self.composer else { return };
        match self.service.comment(&composer.session, composer.anchor.clone(), self.composer_text.text.clone()) {
            Ok(_) => self.close_composer(),
            Err(error) => eprintln!("akshually: {error}"),
        }
    }

    /// Opens the Pull Request in the URL field off the UI thread (the Forge may be slow), then
    /// shows its Session.
    fn open_pull_request(&mut self) {
        let url = self.pr_url.text.trim().to_owned();
        if url.is_empty() {
            return;
        }
        self.pr_status = Some("Opening…".to_owned());
        let service = self.service.clone();
        let opened = |s: &mut State, result: Result<SessionId, akshually_core::Error>| match result {
            Ok(id) => {
                s.active = Some(id);
                s.pr_url = TextEdit::new("");
                s.pr_status = None;
            }
            Err(error) => s.pr_status = Some(error.to_string()),
        };
        match self.handle.clone() {
            Some(handle) => {
                std::thread::spawn(move || {
                    let result = service.open_pull_request(&url);
                    handle.update(move |s: &mut State, _| opened(s, result));
                });
            }
            None => opened(self, service.open_pull_request(&url)),
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
                .child(
                    row()
                        .h(40.0)
                        .w_full()
                        .gap(8.0)
                        .pr(8.0)
                        .items_center()
                        .child(row().key("tabbar").h(40.0).grow(1.0).min_w(0.0).child(tabs))
                        .child(pull_request_opener(k, s)),
                )
                .child(session_header(k, tab))
                .child(col().grow(1.0).min_h(0.0).child(session_diff(k, s, tab)))
        }
        None => col()
            .w_full()
            .h_full()
            .items_center()
            .justify_center()
            .gap(16.0)
            .child(
                text(
                    "No Sessions. Run `akshually review` in a repository to ask for a review, or open a Pull Request.",
                )
                .font(k.ty.ui)
                .color(k.c.text_muted),
            )
            .child(pull_request_opener(k, s)),
    };
    rune_kit::root(body.fill(k.c.bg_base), layer)
}

fn pull_request_opener(k: &Kit, s: &State) -> El {
    let field = TextInput::new(&s.pr_url)
        .placeholder("Open a Pull Request by URL…")
        .width(Some(360.0))
        .on_edit(|s: &mut State| &mut s.pr_url)
        .on_submit(|s: &mut State, _| s.open_pull_request())
        .build(k);
    let status = s.pr_status.clone().map(|status| text(status).font(k.ty.small).color(k.c.text_muted));
    row().gap(8.0).items_center().children(status).child(field)
}

fn session_header(k: &Kit, tab: &SessionTab) -> El {
    let session = &tab.session;
    let base = &session.changeset.base[..session.changeset.base.len().min(8)];
    let drafts = tab.drafts();
    if let Some(pr) = &session.pull_request {
        // Drafts stay in the Session until Publish.
        let close_id = session.id.clone();
        return row()
            .key("session-header")
            .w_full()
            .h(56.0)
            .px(16.0)
            .gap(12.0)
            .items_center()
            .fill(k.c.layer_01)
            .child(text(pr.title.clone()).font(k.ty.ui_medium).color(k.c.text_base))
            .child(text(format!("{} · {} → {base}", pr.author, session.branch)).font(k.ty.mono).color(k.c.text_muted))
            .child(row().grow(1.0))
            .child(Badge::new(format!("{drafts} Drafts")).build(k))
            .child(
                Button::new("Close")
                    .variant(ButtonVariant::Ghost)
                    .on_press(move |s: &mut State| s.act(&close_id, Service::abandon))
                    .build(k),
            );
    }
    let (approve_id, abandon_id, hand_over_id) = (session.id.clone(), session.id.clone(), session.id.clone());
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
        .child(Badge::new(format!("Round {}", session.round)).build(k))
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
        .child(
            Button::new(if drafts == 0 { "Hand over Round".to_owned() } else { format!("Hand over Round ({drafts})") })
                .variant(ButtonVariant::Submit)
                .disabled(drafts == 0)
                .on_press(move |s: &mut State| {
                    if let Err(error) = s.service.hand_over_round(&hand_over_id) {
                        eprintln!("akshually: {error}");
                    }
                })
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
    .annotation({
        let k = k.clone();
        move |s: &State, a: &Annotation| annotation(&k, s, a)
    })
    .on_comment(|s: &mut State, range: LineRange, _: &mut Tx| s.compose(range))
    .build(s, k)
}

fn annotation(k: &Kit, s: &State, a: &Annotation) -> El {
    if a.id == COMPOSER {
        return composer(k, s);
    }
    let thread = s.active_tab().and_then(|t| t.session.threads.iter().find(|t| t.id.as_str() == a.id));
    match thread {
        Some(thread) => thread_card(k, thread),
        None => col(),
    }
}

fn card(k: &Kit) -> El {
    col().w_full().max_w(720.0).padding(12.0).gap(8.0).radius(8.0).fill(k.c.layer_01).border(1.0, k.c.border_weak)
}

fn thread_card(k: &Kit, thread: &Thread) -> El {
    let mut card = card(k);
    for comment in &thread.comments {
        let who = match &comment.author {
            Party::Human => "You",
            Party::Agent => "Agent",
            Party::Other(login) => login,
        };
        let when = match comment.round {
            None if comment.is_draft() => "Draft · goes out with your next Round".to_owned(),
            None => "on the Pull Request".to_owned(),
            Some(round) => format!("Round {round}"),
        };
        card = card
            .child(
                row()
                    .gap(8.0)
                    .items_center()
                    .child(text(who).font(k.ty.ui_medium).color(k.c.text_base))
                    .child(text(format!("{} · {when}", thread.id)).font(k.ty.small).color(k.c.text_faint)),
            )
            .child(text(comment.body.clone()).font(k.ty.body).color(k.c.text_base).wrap());
    }
    card
}

fn composer(k: &Kit, s: &State) -> El {
    card(k)
        .key("composer")
        .child(text("New Comment · a Draft until you hand over the Round").font(k.ty.small).color(k.c.text_faint))
        .child(
            TextArea::new(&s.composer_text)
                .placeholder("Comment…")
                .width(None)
                .auto_grow(2, 12)
                .on_edit(|s: &mut State| &mut s.composer_text)
                .on_submit(|s: &mut State, _| s.save_draft())
                .build(k),
        )
        .child(
            row()
                .w_full()
                .gap(8.0)
                .items_center()
                .child(row().grow(1.0))
                .child(
                    Button::new("Discard")
                        .variant(ButtonVariant::Ghost)
                        .on_press(|s: &mut State| s.close_composer())
                        .build(k),
                )
                .child(
                    Button::new("Save Draft")
                        .variant(ButtonVariant::Submit)
                        .on_press(|s: &mut State| s.save_draft())
                        .build(k),
                ),
        )
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
        pull_request: None,
        changeset,
        round: 2,
        threads: vec![
            sample_thread("T1", 3..=4, "Take the snapshot before building the Round.", Some(1)),
            sample_thread("T2", 5..=5, "Should this return the snapshot error as is?", None),
        ],
    };
    s.apply(vec![session]);
}

fn sample_thread(id: &str, lines: std::ops::RangeInclusive<u32>, body: &str, round: Option<u32>) -> Thread {
    use akshually_core::{Comment, ThreadId};
    Thread {
        id: ThreadId::new(id),
        anchor: Anchor::lines(Side::New, "src/session.rs", lines),
        excerpt: String::new(),
        comments: vec![Comment { author: Party::Human, body: body.into(), round }],
    }
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
