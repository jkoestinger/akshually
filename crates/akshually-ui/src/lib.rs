//! akshually's UI: a thin Rune shell over the core review service (ADR 0001).
//!
//! The view model is rebuilt from the service's plain-data Sessions; every human action goes
//! straight back to the service. No review logic lives here.

use std::sync::Arc;

use akshually_core::{
    Anchor, Changeset, ChangesetView, Collapsed, FileChange, Party, Service, SessionId, SessionView, Side, Thread,
    ThreadId,
};
use rune::{AppSpec, Cx, El, KeyEvent, TextEdit, Tx, col, row, text};
use rune_kit::diff::{self, Annotation, DiffState, DiffView, FileDiff, LineRange, LineRef};
use rune_kit::shell::{TabItem, TabOrientation, TabStrip};
use rune_kit::{Badge, Button, ButtonVariant, Kit, TextArea};

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
        let mut diff = DiffState::new(Vec::new());
        show_files(&mut diff, session.shown());
        let mut tab = SessionTab { session, diff };
        tab.annotate(None);
        tab
    }

    /// Puts each Thread, and the composer when it's on this Session, under its last line.
    fn annotate(&mut self, composer: Option<&Composer>) {
        let changeset = self.session.shown();
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

/// Shows `changeset` in `diff`, with generated and large files collapsed.
fn show_files(diff: &mut DiffState, changeset: &Changeset) {
    diff.set_files(file_diffs(changeset));
    for (at, file) in changeset.files.iter().enumerate() {
        if file.collapsed.is_some() {
            diff.set_collapsed(at, true);
        }
    }
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
        let mut state =
            State { service, tabs: Vec::new(), active: None, composer: None, composer_text: TextEdit::new("") };
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
                    if tab.session.shown() != session.shown() {
                        show_files(&mut tab.diff, session.shown());
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

    fn show(&self, id: &SessionId, view: ChangesetView) {
        if let Err(error) = self.service.show(id, view) {
            eprintln!("akshually: {error}");
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
        let Some(anchor) = anchor_for(tab.session.shown(), range) else { return };
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

    /// `act`, for a human action on one Thread.
    fn act_on_thread(
        &self,
        id: &SessionId,
        thread: &ThreadId,
        action: fn(&Service, &SessionId, &ThreadId) -> Result<(), akshually_core::Error>,
    ) {
        if let Err(error) = action(&self.service, id, thread) {
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
    let shown = session.shown();
    let base = if shown == &session.changeset {
        format!("→ {}", &shown.base[..shown.base.len().min(8)])
    } else {
        "since Session base".to_owned()
    };
    let (approve_id, abandon_id, hand_over_id) = (session.id.clone(), session.id.clone(), session.id.clone());
    let drafts = tab.drafts();
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
        .child(Badge::new(format!("Round {}", session.round)).build(k))
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
    let session = s.active.clone();
    match (thread, session) {
        (Some(thread), Some(session)) if thread.resolved.is_some() => resolved_row(k, thread, session),
        (Some(thread), Some(session)) => thread_card(k, thread, session),
        _ => col(),
    }
}

/// A Resolved Thread, collapsed to one line with a way to reopen it.
fn resolved_row(k: &Kit, thread: &Thread, session: SessionId) -> El {
    let by = match thread.resolved {
        Some(Party::Agent) => "agent",
        _ => "you",
    };
    let first = thread.comments.first().map_or("", |c| c.body.lines().next().unwrap_or(""));
    let count = thread.comments.len();
    let id = thread.id.clone();
    row()
        .w_full()
        .max_w(720.0)
        .px(12.0)
        .py(6.0)
        .gap(8.0)
        .items_center()
        .radius(8.0)
        .border(1.0, k.c.border_weak)
        .child(
            text(format!(
                "✓ Resolved by {by} · {} · “{first}” · {count} Comment{}",
                thread.id,
                if count == 1 { "" } else { "s" }
            ))
            .font(k.ty.small)
            .color(k.c.text_faint),
        )
        .child(row().grow(1.0))
        .child(
            Button::new("Reopen")
                .variant(ButtonVariant::Ghost)
                .on_press(move |s: &mut State| s.act_on_thread(&session, &id, Service::reopen_thread))
                .build(k),
        )
}

fn card(k: &Kit) -> El {
    col().w_full().max_w(720.0).padding(12.0).gap(8.0).radius(8.0).fill(k.c.layer_01).border(1.0, k.c.border_weak)
}

fn thread_card(k: &Kit, thread: &Thread, session: SessionId) -> El {
    let mut card = card(k);
    for comment in &thread.comments {
        let who = match comment.author {
            Party::Human => "You",
            Party::Agent => "Agent",
        };
        let when = match comment.round {
            None => "Draft · goes out with your next Round".to_owned(),
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
    // A Thread still all Drafts hasn't reached the agent yet: nothing to resolve.
    if thread.comments.iter().any(|c| !c.is_draft()) {
        let id = thread.id.clone();
        card = card.child(
            row().w_full().child(row().grow(1.0)).child(
                Button::new("Resolve")
                    .variant(ButtonVariant::Ghost)
                    .on_press(move |s: &mut State| s.act_on_thread(&session, &id, Service::resolve_thread))
                    .build(k),
            ),
        );
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
        since_session_base: Some(Changeset { base: "5e5510nba5e".into(), files: changeset.files[..2].to_vec() }),
        changeset,
        view: ChangesetView::Full,
        round: 2,
        threads: vec![
            sample_thread("T1", 3..=4, "Take the snapshot before building the Round.", Some(1)),
            sample_thread("T2", 5..=5, "Should this return the snapshot error as is?", None),
            sample_thread("T3", 1..=1, "Rename `turn` to `whose_turn`?", Some(1)),
        ],
    };
    let mut session = session;
    let agent = |body: &str| akshually_core::Comment { author: Party::Agent, body: body.into(), round: Some(2) };
    session.threads[0].comments.push(agent("Done: the snapshot is taken first."));
    session.threads[2].comments.push(agent("Renamed."));
    session.threads[2].resolved = Some(Party::Agent);
    s.apply(vec![session]);
}

fn sample_thread(id: &str, lines: std::ops::RangeInclusive<u32>, body: &str, round: Option<u32>) -> Thread {
    use akshually_core::Comment;
    Thread {
        id: ThreadId::new(id),
        anchor: Anchor::lines(Side::New, "src/session.rs", lines),
        excerpt: String::new(),
        comments: vec![Comment { author: Party::Human, body: body.into(), round }],
        resolved: None,
    }
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
