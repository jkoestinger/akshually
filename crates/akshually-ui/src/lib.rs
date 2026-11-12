//! akshually's UI: a thin Rune shell over the core review service (ADR 0001).
//!
//! The view model is rebuilt from the service's plain-data Sessions; every human action goes
//! straight back to the service. No review logic lives here.

use std::sync::Arc;

use akshually_core::{
    Anchor, Changeset, ChangesetView, Collapsed, FileChange, NewComment, Outcome, Party, Service, SessionId,
    SessionView, Side, Thread, ThreadId, Turn,
};
use rune::{AppHandle, AppSpec, Cx, El, KeyEvent, ScrollSpec, TextEdit, Tx, col, row, text};
use rune_kit::diff::{self, Annotation, DiffState, DiffView, FileDiff, LineRange, LineRef};
use rune_kit::shell::{TabItem, TabItemStatus, TabOrientation, TabStrip};
use rune_kit::{Badge, BadgeVariant, Button, ButtonVariant, Checkbox, Kit, Markdown, TextArea, TextInput};

/// The annotation id of the Comment composer (Thread ids are `T1`, `T2`, …).
const COMPOSER: &str = "composer";

mod notifier;
mod theme;

pub use notifier::DesktopNotifier;

/// Whether `rune_args` ask Rune for a headless render (`--snap`, `--tree`, ...) rather than
/// a window. Such a run must not bind the socket or touch the state dir.
pub fn is_headless_run(rune_args: &[String]) -> bool {
    rune::tooling::Args::parse_from(rune_args.to_vec()).headless()
}

/// Runs the app's window over `service` until the app quits. `rune_args` are Rune's own
/// flags, without the program name.
pub fn run(service: Arc<Service>, rune_args: Vec<String>) -> Result<(), String> {
    let spec = AppSpec {
        scenes: &[("session", scene_session), ("threads", scene_threads)],
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
        .keep_running()
        // Notifications are for when the human looks elsewhere.
        .on_focus(|s: &mut State, focused, _| s.service.set_window_focused(focused));
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
    /// The Thread a reply is being written in, at most one in the window.
    reply: Option<(SessionId, ThreadId)>,
    reply_text: TextEdit,
}

/// A Comment being written, before it becomes a Draft.
struct Composer {
    session: SessionId,
    anchor: Anchor,
    /// Where the composer shows in the diff: a line range's last line. File and Review
    /// composers show above the diff instead.
    at: Option<Annotation>,
}

struct SessionTab {
    session: SessionView,
    diff: DiffState,
    /// Outdated Threads the human expanded; the others show as one collapsed line.
    expanded: Vec<ThreadId>,
}

impl SessionTab {
    fn new(session: SessionView) -> Self {
        let mut diff = DiffState::new(Vec::new());
        show_files(&mut diff, session.shown());
        let mut tab = SessionTab { session, diff, expanded: Vec::new() };
        tab.annotate(None);
        tab
    }

    /// Puts each current Thread, and the composer when it's on this Session, under its last
    /// line. Outdated Threads have no line; they show above the diff.
    fn annotate(&mut self, composer: Option<&Composer>) {
        let changeset = self.session.shown();
        let mut notes: Vec<Annotation> = self
            .session
            .threads
            .iter()
            .filter(|t| !t.outdated)
            .filter_map(|t| {
                let Anchor::Lines { path, side, end, .. } = &t.anchor else { return None };
                if *side == Side::Old && self.session.view == ChangesetView::SinceLastRound {
                    // That old side is the last Round's code, not the code the Thread is on.
                    return None;
                }
                let file = changeset.files.iter().position(|f| &f.path == path)?;
                Some(Annotation::new(file, LineRef::new(diff_side(*side), *end), t.id.as_str()))
            })
            .collect();
        notes.extend(composer.filter(|c| c.session == self.session.id).and_then(|c| c.at.clone()));
        self.diff.set_annotations(notes);
    }

    /// What the next Round carries: each Draft, and each Thread the human reopened.
    fn to_hand_over(&self) -> usize {
        let drafts = self.session.threads.iter().flat_map(|t| &t.comments).filter(|c| c.is_draft()).count();
        drafts + self.session.threads.iter().filter(|t| t.reopened).count()
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
        let mut state = State {
            service,
            tabs: Vec::new(),
            active: None,
            composer: None,
            composer_text: TextEdit::new(""),
            pr_url: TextEdit::new(""),
            pr_status: None,
            handle: None,
            reply: None,
            reply_text: TextEdit::new(""),
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
        if self.reply.as_ref().is_some_and(|(session, _)| !self.tabs.iter().any(|t| &t.session.id == session)) {
            self.close_reply();
        }
        self.annotate();
        if self.active_tab().is_none() {
            // The displayed Session ended (or none was): open the first tab, if any.
            if let Some(id) = self.tabs.first().map(|t| t.session.id.clone()) {
                self.open_tab(id);
            } else {
                self.active = None;
            }
        }
    }

    /// Opens the Pull Request in the URL field off the UI thread (the Forge may be slow); the
    /// service then displays its Session.
    fn open_pull_request(&mut self) {
        let url = self.pr_url.text.trim().to_owned();
        if url.is_empty() {
            return;
        }
        self.pr_status = Some("Opening…".to_owned());
        let service = self.service.clone();
        let opened = |s: &mut State, result: Result<SessionId, akshually_core::Error>| match result {
            Ok(id) => {
                s.open_tab(id);
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

    /// Shows the Session's tab and tells the service, which clears its attention.
    fn open_tab(&mut self, id: SessionId) {
        // The Session may have ended meanwhile; its tab goes with the next update.
        let _ = self.service.display(&id);
        self.active = Some(id);
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
        if tab.session.view == ChangesetView::SinceLastRound && range.end.side == diff::Side::Old {
            // Those lines are the last Round's code, which a Thread can't anchor to.
            return;
        }
        let Some(anchor) = anchor_for(tab.session.shown(), range) else { return };
        let at = Annotation::new(range.file, range.end, COMPOSER);
        self.open_composer(anchor, Some(at));
    }

    /// Opens the composer on a whole file (`Some(index)`) or the whole Review (`None`) of the
    /// active Session.
    fn compose_general(&mut self, file: Option<usize>) {
        let Some(tab) = self.active_tab() else { return };
        let anchor = match file {
            Some(at) => match tab.session.shown().files.get(at) {
                Some(f) => Anchor::file(f.path.clone()),
                None => return,
            },
            None => Anchor::Review,
        };
        self.open_composer(anchor, None);
    }

    fn open_composer(&mut self, anchor: Anchor, at: Option<Annotation>) {
        let Some(tab) = self.active_tab() else { return };
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
        let comment = NewComment::from_markdown(&self.composer_text.text);
        match self.service.comment(&composer.session, composer.anchor.clone(), comment) {
            Ok(_) => self.close_composer(),
            Err(error) => eprintln!("akshually: {error}"),
        }
    }

    /// Opens the reply composer in `thread`.
    fn open_reply(&mut self, session: SessionId, thread: ThreadId) {
        self.reply = Some((session, thread));
        self.reply_text = TextEdit::new("");
    }

    fn close_reply(&mut self) {
        self.reply = None;
        self.reply_text = TextEdit::new("");
    }

    /// Turns the reply composer's text into a Draft in its Thread.
    fn save_reply(&mut self) {
        let Some((session, thread)) = &self.reply else { return };
        match self.service.reply_in_thread(session, thread, self.reply_text.text.clone()) {
            Ok(()) => self.close_reply(),
            Err(error) => eprintln!("akshually: {error}"),
        }
    }

    /// Expands or collapses an Outdated Thread.
    fn toggle_outdated(&mut self, session: &SessionId, thread: &ThreadId) {
        let Some(tab) = self.tabs.iter_mut().find(|t| &t.session.id == session) else { return };
        match tab.expanded.iter().position(|t| t == thread) {
            Some(at) => {
                tab.expanded.remove(at);
            }
            None => tab.expanded.push(thread.clone()),
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
                    TabItem::new(t.session.id.as_str(), t.title())
                        .icon(&rune_kit::glyphs::FILE_CODE)
                        .closable(false)
                        .status(tab_status(&t.session))
                }))
                .active(s.active.as_ref().map(SessionId::as_str))
                .viewport_width(cx.layout_size("tabbar").map(|z| z[0]))
                .on_select(|s: &mut State, id: &str, _| {
                    if let Some(id) = s.tabs.iter().find(|t| t.session.id.as_str() == id).map(|t| t.session.id.clone())
                    {
                        s.open_tab(id);
                    }
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
                .children(turn_banner(k, &tab.session))
                .child(disk_banner(k, tab))
                .child(general_threads(k, s, tab))
                .child(outdated_threads(k, tab))
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

/// A Pull Request Session's header. Its Drafts stay in the Session until Publish.
fn pull_request_header(k: &Kit, tab: &SessionTab, pr: &akshually_core::forge::PullRequest) -> El {
    let session = &tab.session;
    let base = &session.changeset.base[..session.changeset.base.len().min(8)];
    let close_id = session.id.clone();
    row()
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
        .child(Badge::new(format!("{} Drafts", tab.to_hand_over())).build(k))
        .child(
            Button::new("Comment on Review")
                .variant(ButtonVariant::Ghost)
                .on_press(|s: &mut State| s.compose_general(None))
                .build(k),
        )
        .child(
            Button::new("Close")
                .variant(ButtonVariant::Ghost)
                .on_press(move |s: &mut State| s.act(&close_id, Service::abandon))
                .build(k),
        )
}

/// The tab's marker: attention when the Session became the human's Turn unseen, a quiet
/// working indicator during the agent's Turn.
/// The tab's marker: attention when the Session became the human's Turn unseen, a dot when
/// files changed on disk, a quiet working indicator during the agent's Turn.
fn tab_status(session: &SessionView) -> Option<TabItemStatus> {
    if session.attention {
        Some(TabItemStatus::Attention)
    } else if !session.changed_on_disk.is_empty() {
        Some(TabItemStatus::Unread)
    } else if session.turn == Turn::Agent {
        Some(TabItemStatus::Busy)
    } else {
        None
    }
}

fn session_header(k: &Kit, tab: &SessionTab) -> El {
    let session = &tab.session;
    if let Some(pr) = &session.pull_request {
        return pull_request_header(k, tab, pr);
    }
    let shown = session.shown();
    let base = if shown == &session.changeset {
        format!("→ {}", &shown.base[..shown.base.len().min(8)])
    } else if session.view == ChangesetView::SinceLastRound {
        "since last Round".to_owned()
    } else {
        "since Session base".to_owned()
    };
    let (approve_id, abandon_id, hand_over_id) = (session.id.clone(), session.id.clone(), session.id.clone());
    let take_back_id = session.id.clone();
    let drafts = tab.to_hand_over();
    let yours = session.turn == Turn::Human;
    let turn = Badge::new(if yours { "Your Turn" } else { "Agent's Turn" });
    // During the agent's Turn the Verdict waits: take the Turn back first (or abandon).
    let decide = if yours {
        Button::new("Approve")
            .variant(ButtonVariant::Contrast)
            .on_press(move |s: &mut State| s.act(&approve_id, Service::approve))
            .build(k)
    } else {
        Button::new("Take Turn back")
            .variant(ButtonVariant::Outline)
            .on_press(move |s: &mut State| s.act(&take_back_id, Service::take_turn_back))
            .build(k)
    };
    let views = [
        ("Since last Round", ChangesetView::SinceLastRound, session.since_last_round.is_some()),
        ("Since Session base", ChangesetView::SinceSessionBase, session.since_session_base.is_some()),
    ];
    let toggle = views.iter().any(|(_, _, available)| *available).then(|| {
        let view_button = |label: &str, view: ChangesetView| {
            let id = session.id.clone();
            let variant = if session.view == view { ButtonVariant::Neutral } else { ButtonVariant::Ghost };
            Button::new(label).variant(variant).on_press(move |s: &mut State| s.show(&id, view)).build(k)
        };
        views
            .iter()
            .filter(|(_, _, available)| *available)
            .fold(row().gap(4.0), |toggle, (label, view, _)| toggle.child(view_button(label, *view)))
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
        .child(if yours { turn.variant(BadgeVariant::Accent) } else { turn }.build(k))
        .child(row().grow(1.0))
        .children(toggle)
        .child(
            Button::new("Comment on Review")
                .variant(ButtonVariant::Ghost)
                .on_press(|s: &mut State| s.compose_general(None))
                .build(k),
        )
        .child(
            Button::new("Abandon")
                .variant(ButtonVariant::Ghost)
                .on_press(move |s: &mut State| s.act(&abandon_id, Service::abandon))
                .build(k),
        )
        .child(decide)
        .child(
            Button::new(if drafts == 0 { "Hand over Round".to_owned() } else { format!("Hand over Round ({drafts})") })
                .variant(ButtonVariant::Submit)
                .disabled(drafts == 0 || !yours)
                .on_press(move |s: &mut State| {
                    if let Err(error) = s.service.hand_over_round(&hand_over_id) {
                        eprintln!("akshually: {error}");
                    }
                })
                .build(k),
        )
}

/// During the agent's Turn: what the human can still do. Once ended: that the agent hasn't
/// collected the outcome yet.
fn turn_banner(k: &Kit, session: &SessionView) -> Option<El> {
    let note = match &session.ended {
        Some(Outcome::Approved) => {
            "Approved. The agent's next `akshually review` receives the Verdict and ends this Session.".to_owned()
        }
        Some(_) => {
            "Abandoned. The agent's next `akshually review` is told to stop, which ends this Session.".to_owned()
        }
        None if session.turn == Turn::Agent => format!(
            "Waiting for the agent to answer Round {}. You can keep reading and drafting: new Drafts go out with \
             Round {}.",
            session.round.saturating_sub(1),
            session.round
        ),
        None => return None,
    };
    Some(
        row()
            .key("turn-banner")
            .w_full()
            .px(16.0)
            .py(8.0)
            .items_center()
            .fill(k.c.layer_02)
            .child(text(note).font(k.ty.small).color(k.c.text_muted).wrap()),
    )
}

/// Offers the files that changed on disk during the human's Turn as a reload; the diff stays
/// as it is until the human takes it.
fn disk_banner(k: &Kit, tab: &SessionTab) -> El {
    let changed = tab.session.changed_on_disk.len();
    if changed == 0 {
        return col();
    }
    let what = if changed == 1 { "1 file".to_owned() } else { format!("{changed} files") };
    let id = tab.session.id.clone();
    row().key("disk-banner").w_full().px(16.0).py(8.0).child(
        row()
            .w_full()
            .px(16.0)
            .py(4.0)
            .gap(12.0)
            .items_center()
            .radius(6.0)
            .fill(k.c.layer_01)
            .border(1.0, k.c.warning.border)
            .child(text(format!("{what} changed on disk during your Turn.")).font(k.ty.ui).color(k.c.text_base))
            .child(row().grow(1.0))
            .child(
                Button::new("Reload")
                    .variant(ButtonVariant::Ghost)
                    .on_press(move |s: &mut State| s.act(&id, Service::reload))
                    .build(k),
            ),
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
        let Some(change) = tab.session.shown().files.get(file) else { return row() };
        let note = file_note(change).unwrap_or_default();
        let viewed = tab.session.viewed.contains(&change.path);
        let (id, path) = (tab.session.id.clone(), change.path.clone());
        row()
            .gap(12.0)
            .items_center()
            .child(text(note).font(kc.ty.small).color(kc.c.text_muted))
            .child(
                Button::new("Comment on file")
                    .variant(ButtonVariant::GhostMuted)
                    .on_press(move |s: &mut State| s.compose_general(Some(file)))
                    .build(&kc),
            )
            .child(
                Checkbox::new(viewed)
                    .label("Viewed")
                    .on_toggle(move |s: &mut State| {
                        if let Err(error) = s.service.mark_viewed(&id, &path, !viewed) {
                            eprintln!("akshually: {error}");
                        }
                    })
                    .build(&kc),
            )
    })
    .annotation({
        let k = k.clone();
        move |s: &State, a: &Annotation| annotation(&k, s, a)
    })
    .on_comment(|s: &mut State, range: LineRange, _: &mut Tx| s.compose(range))
    .build(s, k)
}

/// The Threads on the Review as a whole and on whole files, which have no line in the diff,
/// and the composer when it's writing one.
fn general_threads(k: &Kit, s: &State, tab: &SessionTab) -> El {
    // Outdated File Threads show with the other Outdated Threads instead.
    let general = |t: &Thread| !t.outdated && !matches!(t.anchor, Anchor::Lines { .. });
    let threads: Vec<&Thread> = tab.session.threads.iter().filter(|t| general(t)).collect();
    let composing = s.composer.as_ref().is_some_and(|c| c.session == tab.session.id && c.at.is_none());
    if threads.is_empty() && !composing {
        return col();
    }
    let mut list = col().key("general-threads").w_full().px(16.0).py(8.0).gap(8.0);
    for thread in threads {
        list = list.child(thread_view(k, s, thread, tab.session.id.clone()));
    }
    if composing {
        list = list.child(composer(k, s));
    }
    col().key("general-scroll").w_full().max_h(320.0).scroll(ScrollSpec { stick_bottom: false }).child(list)
}

fn annotation(k: &Kit, s: &State, a: &Annotation) -> El {
    if a.id == COMPOSER {
        return composer(k, s);
    }
    let tab = s.active_tab();
    match tab.and_then(|t| t.session.threads.iter().find(|t| t.id.as_str() == a.id).map(|th| (t, th))) {
        Some((tab, thread)) => thread_view(k, s, thread, tab.session.id.clone()),
        None => col(),
    }
}

/// A Thread of `session`: its card, or one line while it's Resolved.
fn thread_view(k: &Kit, s: &State, thread: &Thread, session: SessionId) -> El {
    if thread.resolved.is_some() {
        resolved_row(k, thread, session)
    } else {
        thread_card(k, thread, Some((s, session)))
    }
}

/// Reopen for a Resolved Thread, Resolve for one that has reached the agent; nothing for a
/// Thread still all Drafts.
fn resolve_button(k: &Kit, thread: &Thread, session: SessionId) -> Option<El> {
    let id = thread.id.clone();
    if thread.resolved.is_some() {
        let reopen = move |s: &mut State| s.act_on_thread(&session, &id, Service::reopen_thread);
        Some(Button::new("Reopen").variant(ButtonVariant::Ghost).on_press(reopen).build(k))
    } else if thread.comments.iter().any(|c| !c.is_draft()) {
        let resolve = move |s: &mut State| s.act_on_thread(&session, &id, Service::resolve_thread);
        Some(Button::new("Resolve").variant(ButtonVariant::Ghost).on_press(resolve).build(k))
    } else {
        None
    }
}

/// `Resolved by agent · `, or nothing for an open Thread.
fn resolved_by(thread: &Thread) -> String {
    match &thread.resolved {
        Some(Party::Agent) => "✓ Resolved by agent · ".to_owned(),
        Some(Party::Human) => "✓ Resolved by you · ".to_owned(),
        Some(Party::Other(login)) => format!("✓ Resolved by {login} · "),
        None => String::new(),
    }
}

/// A Resolved Thread, collapsed to one line with a way to reopen it.
fn resolved_row(k: &Kit, thread: &Thread, session: SessionId) -> El {
    let first = thread.comments.first().map_or("", |c| c.body.lines().next().unwrap_or(""));
    let count = thread.comments.len();
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
                "{}{} · “{first}” · {count} Comment{}",
                resolved_by(thread),
                thread.id,
                if count == 1 { "" } else { "s" }
            ))
            .font(k.ty.small)
            .color(k.c.text_faint),
        )
        .child(row().grow(1.0))
        .children(resolve_button(k, thread, session))
}

fn card(k: &Kit) -> El {
    col().w_full().max_w(720.0).padding(12.0).gap(8.0).radius(8.0).fill(k.c.layer_01).border(1.0, k.c.border_weak)
}

/// A Thread's Comments, with its Reply and Resolve controls when `actions` gives the Session.
fn thread_card(k: &Kit, thread: &Thread, actions: Option<(&State, SessionId)>) -> El {
    let mut card = card(k);
    if let Some(label) = general_label(&thread.anchor) {
        card = card.child(text(label).font(k.ty.mono).color(k.c.text_muted));
    }
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
            .child(Markdown::new(comment.body.clone()).selectable(true).build(k));
        if let Some(suggestion) = &comment.suggestion {
            card = card
                .child(text("Suggested change").font(k.ty.small).color(k.c.text_faint))
                .child(Markdown::new(suggestion_diff(&thread.excerpt, suggestion)).build(k));
        }
    }
    if thread.reopened {
        card = card.child(text("Reopened · goes out with your next Round").font(k.ty.small).color(k.c.text_faint));
    }
    // A Thread still all Drafts goes out whole with the next Round: nothing to reply to yet.
    let sent = thread.comments.iter().any(|c| !c.is_draft());
    if let Some((s, session)) = actions.filter(|_| sent) {
        let replying = s.reply.as_ref().is_some_and(|(at, id)| at == &session && id == &thread.id);
        card = if replying {
            card.child(reply_composer(k, s))
        } else {
            let (reply_session, id) = (session.clone(), thread.id.clone());
            card.child(
                row()
                    .w_full()
                    .gap(8.0)
                    .child(row().grow(1.0))
                    .child(
                        Button::new("Reply")
                            .variant(ButtonVariant::Ghost)
                            .on_press(move |s: &mut State| s.open_reply(reply_session.clone(), id.clone()))
                            .build(k),
                    )
                    .children(resolve_button(k, thread, session)),
            )
        };
    }
    card
}

/// The reply being written in a Thread: a Draft until the Round is handed over.
fn reply_composer(k: &Kit, s: &State) -> El {
    col()
        .key("reply-composer")
        .w_full()
        .gap(8.0)
        .child(
            TextArea::new(&s.reply_text)
                .placeholder("Reply…")
                .width(None)
                .auto_grow(2, 12)
                .on_edit(|s: &mut State| &mut s.reply_text)
                .on_submit(|s: &mut State, _| s.save_reply())
                .build(k),
        )
        .child(
            row()
                .w_full()
                .gap(8.0)
                .items_center()
                .child(text("Markdown · a Draft until you hand over the Round").font(k.ty.small).color(k.c.text_faint))
                .child(row().grow(1.0))
                .child(
                    Button::new("Discard")
                        .variant(ButtonVariant::Ghost)
                        .on_press(|s: &mut State| s.close_reply())
                        .build(k),
                )
                .child(
                    Button::new("Save Draft")
                        .variant(ButtonVariant::Submit)
                        .on_press(|s: &mut State| s.save_reply())
                        .build(k),
                ),
        )
}

/// What a File or Review Thread is on; line Threads sit on their lines instead.
fn general_label(anchor: &Anchor) -> Option<String> {
    match anchor {
        Anchor::Lines { .. } => None,
        Anchor::File { path } => Some(path.clone()),
        Anchor::Review => Some("Whole Review".to_owned()),
    }
}

/// A suggestion as a markdown `diff` fence: the anchored code it replaces, then the code.
fn suggestion_diff(excerpt: &str, suggestion: &str) -> String {
    let removed = excerpt.lines().map(|l| format!("-{l}\n"));
    let added = suggestion.lines().map(|l| format!("+{l}\n"));
    let lines: String = removed.chain(added).collect();
    let longest = lines.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    format!("{fence}diff\n{lines}{fence}\n")
}

/// The Session's Outdated Threads, collapsed to one line each until expanded to show the code
/// they were written against and their Comments.
fn outdated_threads(k: &Kit, tab: &SessionTab) -> El {
    let mut list = col().key("outdated-threads").w_full().px(16.0).gap(8.0);
    for thread in tab.session.threads.iter().filter(|t| t.outdated) {
        let expanded = tab.expanded.contains(&thread.id);
        let place = match &thread.anchor {
            Anchor::Lines { path, side, start, end } if start == end => format!("{path}, {side} line {start}"),
            Anchor::Lines { path, side, start, end } => format!("{path}, {side} lines {start}-{end}"),
            Anchor::File { path } => path.clone(),
            Anchor::Review => "Whole Review".to_owned(),
        };
        let first = thread.comments.first().map(|c| c.body.lines().next().unwrap_or_default()).unwrap_or_default();
        let count = match thread.comments.len() {
            1 => "1 Comment".to_owned(),
            n => format!("{n} Comments"),
        };
        let (session, id) = (tab.session.id.clone(), thread.id.clone());
        let summary = row()
            .w_full()
            .gap(8.0)
            .items_center()
            .child(
                text(format!("Outdated · {}{} · {place} · “{first}” · {count}", resolved_by(thread), thread.id))
                    .font(k.ty.small)
                    .color(k.c.text_muted),
            )
            .child(row().grow(1.0))
            .children(resolve_button(k, thread, tab.session.id.clone()))
            .child(
                Button::new(if expanded { "Hide" } else { "Show" })
                    .variant(ButtonVariant::Ghost)
                    .on_press(move |s: &mut State| s.toggle_outdated(&session, &id))
                    .build(k),
            );
        let mut card = card(k).max_w(f32::INFINITY).child(summary);
        if expanded {
            if !thread.excerpt.is_empty() {
                card = card.child(
                    col()
                        .w_full()
                        .padding(8.0)
                        .radius(4.0)
                        .fill(k.c.bg_base)
                        .child(text(thread.excerpt.trim_end().to_owned()).font(k.ty.mono).color(k.c.text_muted)),
                );
            }
            card = card.child(thread_card(k, thread, None));
        }
        list = list.child(card).pb(8.0);
    }
    list
}

fn composer(k: &Kit, s: &State) -> El {
    card(k)
        .key("composer")
        .child(text(composer_title(s)).font(k.ty.small).color(k.c.text_faint))
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
                .child(text("Markdown · suggestion: ```suggestion").font(k.ty.small).color(k.c.text_faint))
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

fn composer_title(s: &State) -> String {
    let on = s.composer.as_ref().and_then(|c| general_label(&c.anchor));
    match on {
        Some(on) => format!("New Comment on {on} · a Draft until you hand over the Round"),
        None => "New Comment · a Draft until you hand over the Round".to_owned(),
    }
}

fn on_key(s: &mut State, ev: &KeyEvent, tx: &mut Tx) -> bool {
    s.active_tab_mut().is_some_and(|t| t.diff.key(ev, tx))
}

/// `--scene session`: a sample Session, plus tabs for an unseen human's Turn and an agent's
/// Turn, for headless renders.
fn scene_session(s: &mut State) {
    let changeset: Changeset = sample_changeset();
    let session = SessionView {
        id: SessionId::new("sample"),
        repo: "/home/me/akshually".into(),
        branch: "feat/round-snapshots".into(),
        pull_request: None,
        since_session_base: Some(Changeset { base: "5e5510nba5e".into(), files: changeset.files[..2].to_vec() }),
        changeset,
        last_round: None,
        since_last_round: None,
        view: ChangesetView::Full,
        viewed: ["src/round.rs".to_owned()].into(),
        round: 2,
        agent_waiting: true,
        ended: None,
        threads: vec![
            sample_thread("T1", 3..=4, "Take the snapshot before building the Round.", Some(1)),
            sample_thread("T2", 5..=5, "Should this return the snapshot error as is?", None),
            Thread {
                excerpt: "    let round = Round::new(drafts)?;\n".into(),
                outdated: true,
                ..sample_thread("T3", 4..=4, "Why does this return a Result now?", Some(1))
            },
        ],
        turn: Turn::Human,
        attention: false,
        changed_on_disk: Default::default(),
    };
    let other = |id: &str, branch: &str, turn: Turn, attention: bool| SessionView {
        id: SessionId::new(id),
        branch: branch.into(),
        pull_request: None,
        round: 1,
        threads: Vec::new(),
        turn,
        attention,
        ..session.clone()
    };
    let unseen = other("unseen", "fix/inbox-refresh", Turn::Human, true);
    let working = other("working", "feat/publish", Turn::Agent, false);
    s.apply(vec![session, unseen, working]);
}

/// `--scene threads`: Review, file and suggestion Threads, with markdown bodies, an agent
/// reply, a human reply Draft and a Thread the agent resolved, under the banner offering files
/// changed on disk.
fn scene_threads(s: &mut State) {
    use akshually_core::Comment;
    let comment = |body: &str, suggestion: Option<&str>| Comment {
        author: Party::Human,
        body: body.into(),
        round: None,
        suggestion: suggestion.map(Into::into),
    };
    let thread = |id: &str, anchor: Anchor, excerpt: &str, comment: Comment| Thread {
        id: ThreadId::new(id),
        anchor,
        excerpt: excerpt.into(),
        comments: vec![comment],
        resolved: None,
        outdated: false,
        reopened: false,
    };
    let session = SessionView {
        id: SessionId::new("sample"),
        repo: "/home/me/akshually".into(),
        branch: "feat/round-snapshots".into(),
        pull_request: None,
        changeset: sample_changeset(),
        since_session_base: None,
        last_round: None,
        since_last_round: None,
        view: ChangesetView::Full,
        viewed: Default::default(),
        turn: Turn::Human,
        agent_waiting: true,
        ended: None,
        attention: false,
        changed_on_disk: ["src/round.rs".to_owned(), "src/session.rs".to_owned()].into(),
        round: 1,
        threads: vec![
            thread(
                "T1",
                Anchor::Review,
                "",
                comment("Looks good. Two things:\n\n- add a **changelog** entry\n- run `cargo fmt`", None),
            ),
            thread("T2", Anchor::file("src/round.rs"), "", comment("Does `Round` need its own file?", None)),
            thread(
                "T3",
                Anchor::lines(Side::New, "src/session.rs", 3..=3),
                "    let snapshot = self.changeset.snapshot()?;\n",
                comment("Name it for what it holds:", Some("    let before = self.changeset.snapshot()?;\n")),
            ),
            thread(
                "T4",
                Anchor::lines(Side::New, "src/session.rs", 1..=1),
                "",
                comment("Rename `hand_over_round` to `hand_over`?", None),
            ),
        ],
    };
    let mut session = session;
    let agent = |body: &str| Comment { author: Party::Agent, body: body.into(), round: Some(2), suggestion: None };
    for t in &mut session.threads[2..] {
        t.comments[0].round = Some(1);
    }
    session.threads[2].comments.push(agent("Done: it's `before` now."));
    let reply = Comment { author: Party::Human, body: "And the doc comment?".into(), round: None, suggestion: None };
    session.threads[2].comments.push(reply);
    session.threads[3].comments.push(agent("Renamed."));
    session.threads[3].resolved = Some(Party::Agent);
    s.apply(vec![session]);
}

fn sample_thread(id: &str, lines: std::ops::RangeInclusive<u32>, body: &str, round: Option<u32>) -> Thread {
    use akshually_core::Comment;
    Thread {
        id: ThreadId::new(id),
        anchor: Anchor::lines(Side::New, "src/session.rs", lines),
        excerpt: String::new(),
        comments: vec![Comment { author: Party::Human, body: body.into(), round, suggestion: None }],
        resolved: None,
        outdated: false,
        reopened: false,
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
