//! akshually's UI: a thin Rune shell over the core review service (ADR 0001).
//!
//! The view model is rebuilt from the service's plain-data Sessions; every human action goes
//! straight back to the service. No review logic lives here.

#![warn(clippy::unwrap_used)] // Tests may unwrap; production code may not (CODING_STANDARDS.md).

use std::sync::Arc;

use rune::{AppHandle, AppSpec, Color, Cursor, Cx, El, KeyEvent, ScrollSpec, TextEdit, Tx, col, row, text};
use rune_kit::diff::{self, Annotation, DiffState, DiffView, FileDiff, LineRange, LineRef};
use rune_kit::shell::{TabItem, TabItemStatus, TabOrientation, TabStrip};
use rune_kit::{
    Badge, BadgeVariant, Button, ButtonVariant, Checkbox, Dialog, Kit, Markdown, RadioItem, TextArea, TextInput,
};

use akshually_core::forge::{ForgeKind, InboxItem, InboxRole, PullRequestRef};
use akshually_core::{
    Anchor, Changeset, ChangesetView, Collapsed, Comment, FileChange, InboxRow, InboxRowState, InboxView, NewComment,
    Outcome, Party, PublishPlan, Role, Service, SessionId, SessionView, Side, Source, Thread, ThreadId, ThreadRef,
    Turn, Verdict, fence_for,
};

/// The annotation id of the Comment composer (Thread ids are `T1`, `T2`, …).
const COMPOSER: &str = "composer";
/// The Inbox tab's id in the tab strip (Session ids are `s1`, `s2`, …).
const INBOX_TAB: &str = "inbox";
/// How much of a base commit's hash headers show: enough to tell commits apart at a glance.
const SHORT_HASH: usize = 8;

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
        scenes: &[
            ("session", scene_session),
            ("threads", scene_threads),
            ("inbox", scene_inbox),
            ("publish", scene_publish),
            ("co-review", scene_co_review),
        ],
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
        handle.update(move |s: &mut State, _| s.apply(sessions));
    });
    // A Session that becomes the human's Turn, or the human launching akshually again, brings
    // the window back after it was closed.
    let handle = app.handle();
    service.on_show_window(move |token| match token {
        Some(token) => handle.activate(token),
        None if !handle.is_visible() => handle.show(),
        None => {}
    });
    let handle = app.handle();
    service.subscribe_inbox(move |inbox| {
        let inbox = inbox.clone();
        handle.update(move |s: &mut State, _| s.inbox = inbox);
    });
    app.try_run()
}

/// The window's view model.
struct State {
    service: Arc<Service>,
    tabs: Vec<SessionTab>,
    /// The Session whose tab is open; `None` for the Inbox.
    active: Option<SessionId>,
    inbox: InboxView,
    /// An Inbox refresh the human asked for is running.
    refreshing: bool,
    /// The Comment being written, at most one in the window.
    composer: Option<Composer>,
    composer_text: TextEdit,
    /// The "Open a Pull Request by URL" field, and what became of the last URL opened.
    pr_url: TextEdit,
    pr_status: Option<String>,
    /// For work off the UI thread to report back; `None` in headless renders.
    handle: Option<AppHandle<State>>,
    /// The Thread a reply is being written in, at most one in the window.
    reply: Option<ThreadRef>,
    reply_text: TextEdit,
    /// The co-Reviewer's Draft being edited, at most one in the window.
    editing: Option<ThreadRef>,
    edit_text: TextEdit,
    /// The Publish dialog, while it's open.
    publish: Option<Publish>,
    /// The Publish dialog's summary, the review body.
    summary: TextEdit,
    /// The Session the human asked to abandon (or close, for a Pull Request), until they
    /// confirm or cancel.
    confirming_end: Option<SessionId>,
}

/// The Publish dialog of one Pull Request Session.
struct Publish {
    session: SessionId,
    /// What Publish would send, once the Forge said which Verdicts it takes.
    plan: Option<PublishPlan>,
    verdict: Option<Verdict>,
    /// Asking the Forge, or Publishing.
    busy: bool,
    /// Why the last attempt failed.
    error: Option<String>,
}

/// A Comment being written, before it becomes a Draft.
struct Composer {
    session: SessionId,
    anchor: Anchor,
    /// Where the composer shows in the diff: a line range's last line. File and Review
    /// composers show above the diff instead.
    at: Option<Annotation>,
    /// Why the Draft can't be saved as it is: on a Pull Request, lines out of the Forge's diff
    /// are refused as soon as they're picked.
    error: Option<String>,
}

/// One Session's tab: the service's view of it, and the diff and folds that are the UI's own.
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
        let placed = self.session.placed_threads().into_iter();
        let mut notes: Vec<Annotation> = placed
            .map(|p| Annotation::new(p.file, LineRef::new(diff_side(p.side), p.line), p.thread.as_str()))
            .collect();
        notes.extend(composer.filter(|c| c.session == self.session.id).and_then(|c| c.at.clone()));
        self.diff.set_annotations(notes);
    }
}

fn diff_side(side: Side) -> diff::Side {
    match side {
        Side::Old => diff::Side::Old,
        Side::New => diff::Side::New,
    }
}

fn core_side(side: diff::Side) -> Side {
    match side {
        diff::Side::Old => Side::Old,
        diff::Side::New => Side::New,
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
            inbox: InboxView::default(),
            refreshing: false,
            composer: None,
            composer_text: TextEdit::new(""),
            pr_url: TextEdit::new(""),
            pr_status: None,
            handle: None,
            reply: None,
            reply_text: TextEdit::new(""),
            editing: None,
            edit_text: TextEdit::new(""),
            publish: None,
            summary: TextEdit::new(""),
            confirming_end: None,
        };
        let sessions = state.service.sessions();
        state.apply(sessions);
        // The Session the service displays, else the first one, else the Inbox.
        match state.service.displayed().or_else(|| state.tabs.first().map(|t| t.session.id.clone())) {
            Some(id) => state.open_tab(id),
            None => state.open_inbox(),
        }
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
        if self.reply.as_ref().is_some_and(|r| !self.tabs.iter().any(|t| t.session.id == r.session)) {
            self.close_reply();
        }
        if self.publish.as_ref().is_some_and(|p| !self.tabs.iter().any(|t| t.session.id == p.session)) {
            self.publish = None;
        }
        if self.confirming_end.as_ref().is_some_and(|id| !self.tabs.iter().any(|t| &t.session.id == id)) {
            self.confirming_end = None;
        }
        self.annotate();
        if self.active.is_some() && self.active_tab().is_none() {
            // The displayed Session ended.
            self.open_inbox();
        } else if self.active.is_none() && !self.service.inbox_displayed() {
            // Nothing was displayed, so a new Session took the view.
            self.active = self.service.displayed().filter(|id| self.tabs.iter().any(|t| &t.session.id == id));
        }
        // Row states follow the Sessions.
        self.inbox = self.service.inbox();
    }

    /// Opens the Pull Request in the URL field off the UI thread (the Forge may be slow); the
    /// service then displays its Session.
    fn open_pull_request(&mut self) {
        let url = self.pr_url.text.trim().to_owned();
        self.open_url(url);
    }

    /// Opens the Pull Request at `url` off the UI thread, starting or focusing its Session.
    fn open_url(&mut self, url: String) {
        if url.is_empty() {
            return;
        }
        self.pr_status = Some("Opening…".to_owned());
        self.off_ui(
            move |service| service.open_pull_request(&url),
            |s, result| match result {
                Ok(id) => {
                    s.open_tab(id);
                    s.pr_url = TextEdit::new("");
                    s.pr_status = None;
                }
                Err(error) => s.pr_status = Some(error.to_string()),
            },
        );
    }

    /// Shows the Inbox tab and tells the service, which clears its attention.
    fn open_inbox(&mut self) {
        self.service.display_inbox();
        self.active = None;
        self.inbox = self.service.inbox();
    }

    /// Asks the Forges for the Inbox off the UI thread; the service's update shows it.
    fn refresh_inbox(&mut self) {
        let Some(handle) = self.handle.clone() else { return };
        if self.refreshing {
            return;
        }
        self.refreshing = true;
        let service = self.service.clone();
        std::thread::spawn(move || {
            service.refresh_inbox();
            handle.update(|s: &mut State, _| s.refreshing = false);
        });
    }

    /// Runs `work` off the UI thread (the Forge may be slow), then `done` with its result on
    /// the UI thread. Headless renders have no handle and run it in place.
    fn off_ui<T: Send + 'static>(
        &mut self,
        work: impl FnOnce(&Service) -> T + Send + 'static,
        done: impl FnOnce(&mut State, T) + Send + 'static,
    ) {
        let service = self.service.clone();
        match self.handle.clone() {
            Some(handle) => {
                std::thread::spawn(move || {
                    let result = work(&service);
                    handle.update(move |s: &mut State, _| done(s, result));
                });
            }
            None => done(self, work(&service)),
        }
    }

    /// Opens the Publish dialog of `id`, asking the Forge which Verdicts it takes.
    fn open_publish(&mut self, id: SessionId) {
        self.publish = Some(Publish { session: id.clone(), plan: None, verdict: None, busy: true, error: None });
        self.summary = TextEdit::new("");
        self.off_ui(
            move |service| service.publish_plan(&id),
            |s, plan| {
                let Some(publish) = &mut s.publish else { return };
                publish.busy = false;
                match plan {
                    Ok(plan) => {
                        // A single choice is made already.
                        publish.verdict = (plan.verdicts.len() == 1).then(|| plan.verdicts[0]);
                        publish.plan = Some(plan);
                    }
                    Err(error) => publish.error = Some(error.to_string()),
                }
            },
        );
    }

    /// Closes the Publish dialog, except while Publishing, whose result the dialog shows. Asking
    /// the Forge which Verdicts it takes can be cancelled.
    fn close_publish(&mut self) {
        if self.publish.as_ref().is_some_and(|p| !p.busy || p.plan.is_none()) {
            self.publish = None;
        }
    }

    /// Publishes the dialog's Session with its Verdict and summary.
    fn publish(&mut self) {
        let Some(publish) = &mut self.publish else { return };
        let Some(verdict) = publish.verdict.filter(|_| !publish.busy) else { return };
        publish.busy = true;
        publish.error = None;
        let (id, summary) = (publish.session.clone(), self.summary.text.clone());
        self.off_ui(
            move |service| service.publish(&id, verdict, &summary),
            |s, published| match published {
                Ok(_) => s.publish = None,
                Err(error) => {
                    if let Some(publish) = &mut s.publish {
                        publish.busy = false;
                        publish.error = Some(error.to_string());
                    }
                }
            },
        );
    }

    /// Shows the Session's tab and tells the service, which clears its attention.
    fn open_tab(&mut self, id: SessionId) {
        // The Session may have ended meanwhile; its tab goes with the next update.
        let _ = self.service.display(&id);
        self.active = Some(id);
    }

    fn show(&self, id: &SessionId, view: ChangesetView) {
        report(self.service.show(id, view));
    }

    fn annotate(&mut self) {
        for tab in &mut self.tabs {
            tab.annotate(self.composer.as_ref());
        }
    }

    /// Opens the composer on `range` of the active Session.
    fn compose(&mut self, range: LineRange) {
        let Some(tab) = self.active_tab() else { return };
        let (start, end) =
            ((core_side(range.start.side), range.start.line), (core_side(range.end.side), range.end.line));
        let Some(anchor) = tab.session.anchor_for_selection(range.file, start, end) else { return };
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
        let error = outside_diff(&tab.session, &anchor);
        self.composer = Some(Composer { session: tab.session.id.clone(), anchor, at, error });
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
            Err(error) => {
                if let Some(composer) = &mut self.composer {
                    composer.error = Some(error.to_string());
                }
            }
        }
    }

    /// Moves the composer, text and all, from lines the Forge refuses to their whole file.
    fn compose_on_file(&mut self) {
        let Some(composer) = &mut self.composer else { return };
        let Anchor::Lines { path, .. } = &composer.anchor else { return };
        composer.anchor = Anchor::file(path.clone());
        composer.at = None;
        composer.error = None;
        self.annotate();
    }

    /// Opens the reply composer in `thread`.
    fn open_reply(&mut self, thread: ThreadRef) {
        self.reply = Some(thread);
        self.reply_text = TextEdit::new("");
    }

    fn close_reply(&mut self) {
        self.reply = None;
        self.reply_text = TextEdit::new("");
    }

    /// Turns the reply composer's text into a Draft in its Thread.
    fn save_reply(&mut self) {
        let Some(thread) = &self.reply else { return };
        match self.service.reply_in_thread(thread, self.reply_text.text.clone()) {
            Ok(()) => self.close_reply(),
            error => report(error),
        }
    }

    /// Opens the editor on the co-Reviewer's Draft `thread`, with what it says now.
    fn open_edit(&mut self, thread: ThreadRef) {
        let tab = self.tabs.iter().find(|t| t.session.id == thread.session);
        let draft = tab.and_then(|t| t.session.threads.iter().find(|th| th.id == thread.thread)?.comments.first());
        let Some(draft) = draft else { return };
        self.edit_text = TextEdit::new(draft_markdown(&draft.body, draft.suggestion.as_deref()));
        self.editing = Some(thread);
    }

    fn close_edit(&mut self) {
        self.editing = None;
        self.edit_text = TextEdit::new("");
    }

    /// Saves the edited co-Reviewer's Draft, which keeps it as the human's.
    fn save_edit(&mut self) {
        let Some(thread) = &self.editing else { return };
        match self.service.edit_draft(thread, NewComment::from_markdown(&self.edit_text.text)) {
            Ok(()) => self.close_edit(),
            error => report(error),
        }
    }

    /// Expands or collapses an Outdated Thread.
    fn toggle_outdated(&mut self, thread: &ThreadRef) {
        let Some(tab) = self.tabs.iter_mut().find(|t| t.session.id == thread.session) else { return };
        match tab.expanded.iter().position(|t| t == &thread.thread) {
            Some(at) => {
                tab.expanded.remove(at);
            }
            None => tab.expanded.push(thread.thread.clone()),
        }
    }

    fn active_tab(&self) -> Option<&SessionTab> {
        self.tabs.iter().find(|t| Some(&t.session.id) == self.active.as_ref())
    }

    fn active_tab_mut(&mut self) -> Option<&mut SessionTab> {
        self.tabs.iter_mut().find(|t| Some(&t.session.id) == self.active.as_ref())
    }

    /// Runs a human action on the service; the service's update then refreshes the view.
    fn act(&self, id: &SessionId, action: SessionAction) {
        report(action(&self.service, id));
    }

    /// Reloads a Session off the UI thread, as a Pull Request reloads from its Forge and local
    /// work through `git`, either of which may be slow; the service's update then refreshes the view.
    fn reload(&self, id: &SessionId) {
        let (service, id) = (self.service.clone(), id.clone());
        let reload = move || report(service.reload(&id));
        match self.handle {
            Some(_) => drop(std::thread::spawn(reload)),
            None => reload(),
        }
    }

    /// `act`, for a human action on one Thread.
    fn act_on_thread(&self, thread: &ThreadRef, action: fn(&Service, &ThreadRef) -> Result<(), akshually_core::Error>) {
        report(action(&self.service, thread));
    }
}

/// A human action on one Session.
type SessionAction = fn(&Service, &SessionId) -> Result<(), akshually_core::Error>;

/// Reports a human action the service refused. The UI offers only what's allowed, so this is
/// for races (the Session changed meanwhile); there's nowhere better to show it yet.
fn report(result: Result<(), akshually_core::Error>) {
    if let Err(error) = result {
        eprintln!("akshually: {error}");
    }
}

/// The first line of a Thread's first Comment, which names it in one-line summaries.
fn first_line(thread: &Thread) -> &str {
    thread.comments.first().map_or("", |c| c.body.lines().next().unwrap_or(""))
}

/// The window: the tab strip, then the open Session or the Inbox, and any dialog above them.
fn view(s: &State, cx: &mut Cx<State>) -> El {
    // Rune keeps `system_dark` current with the OS setting and redraws when it changes.
    let k = theme::kit(cx.system_dark());
    let mut layer = Vec::new();
    // Always first. Not `pinned`: the strip shows pinned tabs as an icon alone.
    let inbox_tab = TabItem::new(INBOX_TAB, format!("Inbox {}", s.inbox.rows.len()))
        .closable(false)
        .status(s.inbox.attention.then_some(TabItemStatus::Attention));
    let tabs = TabStrip::new("tabs", TabOrientation::Horizontal)
        .tabs(std::iter::once(inbox_tab).chain(s.tabs.iter().map(|t| {
            TabItem::new(t.session.id.as_str(), t.session.title())
                .icon(&rune_kit::glyphs::FILE_CODE)
                .closable(false)
                .status(tab_status(&t.session))
        })))
        .active(Some(s.active.as_ref().map_or(INBOX_TAB, SessionId::as_str)))
        .viewport_width(cx.layout_size("tabbar").map(|z| z[0]))
        .on_select(|s: &mut State, id: &str, _| {
            if id == INBOX_TAB {
                s.open_inbox();
            } else if let Some(id) = s.tabs.iter().find(|t| t.session.id.as_str() == id).map(|t| t.session.id.clone()) {
                s.open_tab(id);
            }
        })
        .build_layered(k, &mut layer);
    let tab_bar = row()
        .h(40.0)
        .w_full()
        .gap(8.0)
        .pr(8.0)
        .items_center()
        .child(row().key("tabbar").h(40.0).grow(1.0).min_w(0.0).child(tabs));
    let agent = theme::tokens(cx.system_dark()).color("state-agent");
    let body = match s.active_tab() {
        Some(tab) => col()
            .w_full()
            .h_full()
            .min_h(0.0)
            .child(tab_bar.child(pull_request_opener(k, s)))
            .child(session_header(k, tab))
            .children(end_confirm(k, s, &tab.session))
            .children(turn_banner(k, &tab.session))
            .child(disk_banner(k, tab))
            .child(push_banner(k, tab))
            .child(general_threads(k, s, tab, agent))
            .child(outdated_threads(k, tab))
            .child(col().grow(1.0).min_h(0.0).child(session_diff(k, s, tab, agent))),
        None => {
            let draft = theme::tokens(cx.system_dark()).color("state-draft");
            col().w_full().h_full().min_h(0.0).child(tab_bar).child(inbox_view(k, s, draft))
        }
    };
    publish_dialog(k, s, &mut layer);
    rune_kit::root(body.fill(k.c.bg_base), layer)
}

/// The Inbox tab: open Pull Requests where the human is a requested reviewer or assignee, on
/// every configured host. Opening a row starts or focuses its Session.
fn inbox_view(k: &Kit, s: &State, draft: Color) -> El {
    let now = now();
    let hosts: Vec<String> =
        akshually_core::forge::served_hosts(&s.service.settings().hosts).into_iter().map(|h| h.host).collect();
    let refreshed = match (s.refreshing, s.inbox.refreshed_at) {
        (true, _) => " · refreshing…".to_owned(),
        (false, Some(at)) => format!(" · refreshed {}", ago(now - at)),
        (false, None) => String::new(),
    };
    let subtitle =
        format!("Pull Requests where you are a requested reviewer or assignee · {}{refreshed}", hosts.join(", "));
    let header = row()
        .key("inbox-header")
        .w_full()
        .gap(12.0)
        .items_center()
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(4.0)
                .child(text("Inbox").font(k.ty.body_medium).color(k.c.text_base))
                .child(text(subtitle).font(k.ty.small).color(k.c.text_muted)),
        )
        .child(pull_request_opener(k, s))
        .child(
            Button::new("Refresh")
                .variant(ButtonVariant::Outline)
                .disabled(s.refreshing)
                .on_press(|s: &mut State| s.refresh_inbox())
                .build(k),
        );
    let errors = s.inbox.errors.iter().map(|e| text(e.clone()).font(k.ty.small).color(k.c.danger.fg).wrap());
    let mut list = col().key("inbox-rows").w_full().border_sides([1.0, 0.0, 0.0, 0.0], k.c.border_base);
    for inbox_row in &s.inbox.rows {
        list = list.child(inbox_row_view(k, inbox_row, now, draft));
    }
    let empty = s.inbox.rows.is_empty().then(|| {
        let note = if s.inbox.refreshed_at.is_some() { "Nothing waiting for you." } else { "Not refreshed yet." };
        text(note).font(k.ty.ui).color(k.c.text_muted)
    });
    col()
        .key("inbox")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .scroll(ScrollSpec { stick_bottom: false })
        .child(col().w_full().px(24.0).py(16.0).gap(16.0).child(header).children(errors).child(list).children(empty))
}

fn inbox_row_view(k: &Kit, inbox_row: &InboxRow, now: i64, draft: Color) -> El {
    let item = &inbox_row.item;
    let (state, color) = match &inbox_row.state {
        InboxRowState::New => ("New".to_owned(), k.c.warning.fg),
        InboxRowState::Updated => ("Pull Request updated".to_owned(), k.c.warning.fg),
        InboxRowState::Seen => ("—".to_owned(), k.c.text_muted),
        InboxRowState::SessionOpen { drafts, .. } => (
            match drafts {
                0 => "Session open".to_owned(),
                1 => "Session open · 1 Draft".to_owned(),
                n => format!("Session open · {n} Drafts"),
            },
            draft,
        ),
    };
    let unseen = matches!(inbox_row.state, InboxRowState::New | InboxRowState::Updated);
    let dot = row().w(8.0).h(8.0).radius(4.0).fill(if unseen { k.c.warning.fg } else { Color::TRANSPARENT });
    let role = match item.role {
        InboxRole::Reviewer => "Requested reviewer",
        InboxRole::Assignee => "Assignee",
    };
    let url = item.url.clone();
    row()
        .key(format!("inbox-{}", item.url))
        .w_full()
        .px(12.0)
        .py(12.0)
        .gap(12.0)
        .items_center()
        .border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_base)
        .hover(k.c.hover)
        .cursor(Cursor::Pointer)
        .on_press(move |s: &mut State| s.open_url(url.clone()))
        .child(dot)
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(4.0)
                .child(text(item.title.clone()).font(k.ty.ui_medium).color(k.c.text_base))
                .child(text(inbox_meta(item, now)).font(k.ty.small).color(k.c.text_muted)),
        )
        .child(text(role).font(k.ty.small).color(k.c.text_muted))
        .child(row().w(200.0).justify_end().child(text(state).font(k.ty.small).color(color)))
}

/// `GitHub · acme/web #482 · maria · opened 2 h ago`, or `GitLab · gitlab.example.com ·
/// platform/infra !97 · …`: hosts other than github.com and gitlab.com are named.
fn inbox_meta(item: &InboxItem, now: i64) -> String {
    let PullRequestRef { host, repo, number } = &item.id;
    let sigil = item.kind.sigil();
    let forge = match item.kind {
        ForgeKind::GitHub => "GitHub",
        ForgeKind::GitLab => "GitLab",
    };
    let place = if host == "github.com" || host == "gitlab.com" {
        format!("{repo} {sigil}{number}")
    } else {
        format!("{host} · {repo} {sigil}{number}")
    };
    format!("{forge} · {place} · {} · opened {}", item.author, ago(now - item.created_at))
}

/// Roughly how long ago `seconds` ago was: `just now`, `5 min ago`, `2 h ago`, `3 d ago`.
fn ago(seconds: i64) -> String {
    match seconds {
        ..60 => "just now".to_owned(),
        60..3600 => format!("{} min ago", seconds / 60),
        3600..86_400 => format!("{} h ago", seconds / 3600),
        _ => format!("{} d ago", seconds / 86_400),
    }
}

/// Seconds since the Unix epoch.
fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
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
    let base = &session.changeset.base[..session.changeset.base.len().min(SHORT_HASH)];
    let (close_id, publish_id) = (session.id.clone(), session.id.clone());
    let published = session.ended.is_some();
    row()
        .key("session-header")
        .w_full()
        .h(56.0)
        .px(16.0)
        .gap(12.0)
        .items_center()
        .fill(k.c.layer_01)
        .child(text(pr.title.clone()).font(k.ty.ui_medium).color(k.c.text_base))
        .child(text(format!("{} · {} → {base}", pr.author, session.branch())).font(k.ty.mono).color(k.c.text_muted))
        .child(row().grow(1.0))
        .child(Badge::new(format!("{} Drafts", tab.session.drafts())).build(k))
        .child(
            Button::new("Comment on Review")
                .variant(ButtonVariant::Ghost)
                .on_press(|s: &mut State| s.compose_general(None))
                .build(k),
        )
        .child(
            Button::new("Close")
                .variant(ButtonVariant::Ghost)
                .on_press(move |s: &mut State| match published {
                    true => s.act(&close_id, Service::close),
                    // Unpublished Drafts go with it: ask first.
                    false => s.confirming_end = Some(close_id.clone()),
                })
                .build(k),
        )
        .children(session.agent.is_some().then(|| hand_over_button(k, tab)))
        .child(
            Button::new("Publish…")
                .variant(ButtonVariant::Submit)
                .disabled(published)
                .on_press(move |s: &mut State| s.open_publish(publish_id.clone()))
                .build(k),
        )
}

/// Hands the next Round over: in a co-review, the human's questions on the agent's Drafts.
fn hand_over_button(k: &Kit, tab: &SessionTab) -> El {
    let session = &tab.session;
    let id = session.id.clone();
    let count = session.to_hand_over();
    let label = match (session.is_co_review(), count) {
        (true, 0) => "Hand over questions".to_owned(),
        (true, n) => format!("Hand over questions ({n})"),
        (false, 0) => "Hand over Round".to_owned(),
        (false, n) => format!("Hand over Round ({n})"),
    };
    Button::new(label)
        .variant(ButtonVariant::Submit)
        .disabled(count == 0 || session.turn != Turn::Human || session.ended.is_some())
        .on_press(move |s: &mut State| report(s.service.hand_over_round(&id).map(drop)))
        .build(k)
}

/// The Publish dialog: what will be sent and what won't, the Verdict and the summary.
fn publish_dialog(k: &Kit, s: &State, layer: &mut Vec<El>) {
    let Some(publish) = &s.publish else { return };
    let Some(tab) = s.tabs.iter().find(|t| t.session.id == publish.session) else { return };
    let Some(pr) = tab.session.pull_request() else { return };
    let mut body = col().w_full().gap(12.0).px(16.0).pb(16.0);
    if let Some(plan) = &publish.plan {
        let mut list = col().key("publish-drafts").w_full().gap(8.0);
        for (thread, kept) in plan.kept.iter().map(|t| (t, true)).chain(plan.discarded.iter().map(|t| (t, false))) {
            list = list.child(publish_row(k, thread, kept));
        }
        let resolutions = plan.resolve.iter().map(|t| (t, "Resolve")).chain(plan.reopen.iter().map(|t| (t, "Reopen")));
        for (thread, what) in resolutions {
            list = list.child(resolution_row(k, thread, what));
        }
        if [&plan.kept, &plan.discarded, &plan.resolve, &plan.reopen].iter().all(|l| l.is_empty()) {
            list =
                list.child(text("No Drafts: only the Verdict and summary go out.").font(k.ty.ui).color(k.c.text_muted));
        }
        let verdicts = Verdict::ALL.iter().filter(|v| plan.verdicts.contains(v)).fold(row().gap(8.0), |row, &v| {
            row.child(
                RadioItem::new(verdict_label(v), publish.verdict == Some(v))
                    .on_select(move |s: &mut State| {
                        if let Some(p) = &mut s.publish {
                            p.verdict = Some(v);
                        }
                    })
                    .build(k)
                    .key(format!("verdict-{v}"))
                    .grow(1.0),
            )
        });
        let own = plan.verdicts == [Verdict::Feedback];
        body = body
            .child(list)
            .child(text("VERDICT").font(k.ty.small).color(k.c.text_faint))
            .child(verdicts)
            .children(own.then(|| {
                text("Your own Pull Request: the Forge takes feedback only.").font(k.ty.small).color(k.c.text_muted)
            }))
            .child(
                TextArea::new(&s.summary)
                    .placeholder("Summary (optional, markdown): the review body")
                    .width(None)
                    .auto_grow(2, 8)
                    .on_edit(|s: &mut State| &mut s.summary)
                    .build(k),
            )
            .child(text("Feedback is published as the Forge's \"Comment\".").font(k.ty.small).color(k.c.text_faint));
    } else if publish.busy {
        body = body.child(text("Asking the Forge…").font(k.ty.ui).color(k.c.text_muted));
    }
    if let Some(error) = &publish.error {
        body = body.child(text(error.clone()).font(k.ty.small).color(k.c.danger.fg).wrap());
    }
    let sent = publish.plan.as_ref().map_or(0, |p| p.kept.len());
    let label = match publish.verdict {
        _ if publish.busy && publish.plan.is_some() => "Publishing…".to_owned(),
        Some(v) if sent > 0 => {
            format!("Publish {sent} Comment{} · {}", if sent == 1 { "" } else { "s" }, verdict_label(v))
        }
        Some(v) => format!("Publish · {}", verdict_label(v)),
        None => "Publish".to_owned(),
    };
    body = body.child(
        row()
            .w_full()
            .gap(8.0)
            .items_center()
            .child(row().grow(1.0))
            .child(
                Button::new("Cancel")
                    .variant(ButtonVariant::Ghost)
                    .on_press(|s: &mut State| s.close_publish())
                    .build(k),
            )
            .child(
                Button::new(label)
                    .variant(ButtonVariant::Submit)
                    .disabled(publish.verdict.is_none() || publish.busy)
                    .on_press(|s: &mut State| s.publish())
                    .build(k),
            ),
    );
    Dialog::new("publish", "Publish review")
        .description(format!(
            "Sent to {} as one review on {} #{}, under your name.",
            pr.id.host, pr.id.repo, pr.id.number
        ))
        .open(true)
        .width(620.0)
        .fit()
        .click_outside(false)
        .on_close(|s: &mut State| s.close_publish())
        .child(body)
        .build(k, layer);
}

/// One Thread's Drafts in the Publish dialog, ticked when they're sent.
fn publish_row(k: &Kit, thread: &Thread, kept: bool) -> El {
    let on = publish_place(thread);
    let first = first_line(thread);
    let on = if thread.forge.is_some() { format!("Reply in {} on {on}", thread.id) } else { on };
    let what = if kept { format!("{on} — {first}") } else { format!("{on} — {first} (not sent)") };
    let suggestion = thread.comments.iter().any(|c| c.suggestion.is_some());
    row()
        .w_full()
        .gap(8.0)
        .items_center()
        .child(Checkbox::new(kept).disabled(true).build(k))
        .child(text(what).font(k.ty.ui).color(if kept { k.c.text_base } else { k.c.text_faint }).grow(1.0).min_w(0.0))
        .children(suggestion.then(|| text("suggestion").font(k.ty.small).color(k.c.text_faint)))
}

/// Where a Thread is, as the Publish dialog names it.
fn publish_place(thread: &Thread) -> String {
    match &thread.anchor {
        Anchor::Lines { path, side: Side::New, start, end } if start == end => format!("{path}:{end}"),
        Anchor::Lines { path, side: Side::New, start, end } => format!("{path}:{start}-{end}"),
        Anchor::Lines { path, side: Side::Old, start, end } => format!("{path}, old {start}-{end}"),
        anchor => general_label(anchor).unwrap_or_default(),
    }
}

/// A Thread Publish resolves or reopens on the Forge (`what`), in the Publish dialog.
fn resolution_row(k: &Kit, thread: &Thread, what: &str) -> El {
    let first = first_line(thread);
    let line = format!("{what} {} on {} — {first}", thread.id, publish_place(thread));
    row()
        .w_full()
        .gap(8.0)
        .items_center()
        .child(Checkbox::new(true).disabled(true).build(k))
        .child(text(line).font(k.ty.ui).color(k.c.text_base).grow(1.0).min_w(0.0))
}

fn verdict_label(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Approve => "Approve",
        Verdict::RequestChanges => "Request changes",
        Verdict::Feedback => "Feedback",
    }
}

/// The tab's marker: attention when the Session became the human's Turn unseen, a dot when
/// files changed on disk or the Pull Request received pushes, a quiet working indicator during
/// the agent's Turn.
fn tab_status(session: &SessionView) -> Option<TabItemStatus> {
    if session.attention {
        Some(TabItemStatus::Attention)
    } else if !session.changed_on_disk.is_empty() || session.pull_request_update.is_some() {
        Some(TabItemStatus::Unread)
    } else if session.turn == Turn::Agent {
        Some(TabItemStatus::Busy)
    } else {
        None
    }
}

/// A Session's header: its branch and base, Round and Turn, the Changeset views it offers, and
/// the actions that end it or hand the Turn over. A Pull Request Session has its own header.
fn session_header(k: &Kit, tab: &SessionTab) -> El {
    let session = &tab.session;
    if let Some(pr) = session.pull_request() {
        return pull_request_header(k, tab, pr);
    }
    let shown = session.shown();
    let base = if shown == &session.changeset {
        format!("→ {}", &shown.base[..shown.base.len().min(SHORT_HASH)])
    } else if session.view == ChangesetView::SinceLastRound {
        "since last Round".to_owned()
    } else {
        "since Session base".to_owned()
    };
    let (approve_id, abandon_id) = (session.id.clone(), session.id.clone());
    let take_back_id = session.id.clone();
    let yours = session.turn == Turn::Human;
    let co_review = session.is_co_review();
    let turn = Badge::new(if yours { "Your Turn" } else { "Agent's Turn" });
    // During the agent's Turn the Verdict waits: take the Turn back first (or abandon). A
    // co-review of local work has no Verdict.
    let ended = session.ended.is_some();
    let decide = if ended || (yours && co_review) {
        None
    } else if yours {
        Button::new("Approve")
            .variant(ButtonVariant::Contrast)
            .on_press(move |s: &mut State| s.act(&approve_id, Service::approve))
            .build(k)
            .into()
    } else {
        Button::new("Take Turn back")
            .variant(ButtonVariant::Outline)
            .on_press(move |s: &mut State| s.act(&take_back_id, Service::take_turn_back))
            .build(k)
            .into()
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
        .child(text(session.branch().to_owned()).font(k.ty.mono).color(k.c.text_base))
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
            // Closing a co-review of local work is its normal end, not Abandoned; a Session that
            // has its outcome already just closes.
            match (ended, co_review) {
                (true, _) => Button::new("Close").on_press(move |s: &mut State| s.act(&abandon_id, Service::close)),
                (false, true) => {
                    Button::new("Close").on_press(move |s: &mut State| s.act(&abandon_id, Service::abandon))
                }
                (false, false) => {
                    Button::new("Abandon").on_press(move |s: &mut State| s.confirming_end = Some(abandon_id.clone()))
                }
            }
            .variant(ButtonVariant::Ghost)
            .build(k),
        )
        .children(decide)
        .child(hand_over_button(k, tab))
}

/// Asks the human to confirm abandoning a Session (or closing a Pull Request Session before
/// Publish), which can't be undone. Cancel keeps everything.
fn end_confirm(k: &Kit, s: &State, session: &SessionView) -> Option<El> {
    if s.confirming_end.as_ref() != Some(&session.id) {
        return None;
    }
    let (question, label, end): (String, _, SessionAction) = match session.pull_request() {
        Some(_) => {
            let agent = if session.agent.is_some() { " The co-Reviewer is told to stop." } else { "" };
            (format!("Close this Pull Request? Your Drafts are discarded.{agent}"), "Close", Service::close)
        }
        None => ("Abandon this Session? The agent is told to stop.".to_owned(), "Abandon", Service::abandon),
    };
    let id = session.id.clone();
    Some(
        row()
            .key("end-confirm")
            .w_full()
            .px(16.0)
            .py(8.0)
            .gap(12.0)
            .items_center()
            .fill(k.c.layer_02)
            .child(text(question).font(k.ty.ui).color(k.c.text_base))
            .child(row().grow(1.0))
            .child(
                Button::new("Cancel")
                    .variant(ButtonVariant::Ghost)
                    .on_press(|s: &mut State| s.confirming_end = None)
                    .build(k),
            )
            .child(
                Button::new(label)
                    .variant(ButtonVariant::Danger)
                    .on_press(move |s: &mut State| {
                        s.confirming_end = None;
                        s.act(&id, end);
                    })
                    .build(k),
            ),
    )
}

/// During the agent's Turn: what the human can still do. Once ended: that the agent hasn't
/// collected the outcome yet.
fn turn_banner(k: &Kit, session: &SessionView) -> Option<El> {
    let note = match &session.ended {
        Some(Outcome::Approved) => {
            "Approved. The agent's next `akshually review` receives the Verdict and ends this Session, or close it \
             now."
                .to_owned()
        }
        Some(Outcome::Published { verdict, url }) => {
            format!("Published with the Verdict {verdict}: {url}. Close this Session when you're done.")
        }
        Some(Outcome::Closed) => {
            "Closed. The agent's next `akshually review` is told the co-review is over, which ends this Session, \
             or close it now."
                .to_owned()
        }
        Some(Outcome::Abandoned | Outcome::Round(_)) => {
            "Abandoned. The agent's next `akshually review` is told to stop, which ends this Session, or close it \
             now."
                .to_owned()
        }
        None => match session.answering_round() {
            Some(answering) => format!(
                "Waiting for the agent to answer Round {answering}. You can keep reading and drafting: new Drafts go \
                 out with Round {}.",
                session.round
            ),
            // A co-Reviewer that opened the Session hasn't asked for the human yet.
            None if session.turn == Turn::Agent => "The agent is still reviewing: its Drafts show here as it adds \
                                                    them. You can read and draft meanwhile."
                .to_owned(),
            None => return None,
        },
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
                    .on_press(move |s: &mut State| s.reload(&id))
                    .build(k),
            ),
    )
}

/// Offers the commits pushed to the Pull Request since it was fetched as a reload; the diff
/// stays as it is until the human takes it.
fn push_banner(k: &Kit, tab: &SessionTab) -> El {
    let Some(update) = &tab.session.pull_request_update else { return col() };
    let who = &update.pull_request.author;
    let what = match update.commits {
        Some(1) => format!("{who} pushed 1 commit"),
        Some(n) => format!("{who} pushed {n} commits"),
        None => format!("{who} pushed new commits"),
    };
    let id = tab.session.id.clone();
    row().key("push-banner").w_full().px(16.0).py(8.0).child(
        row()
            .w_full()
            .px(16.0)
            .py(4.0)
            .gap(12.0)
            .items_center()
            .radius(6.0)
            .fill(k.c.layer_01)
            .border(1.0, k.c.warning.border)
            .child(
                text(format!(
                    "{what} since you opened this Pull Request. Your Drafts are kept; some may become Outdated."
                ))
                .font(k.ty.ui)
                .color(k.c.text_base),
            )
            .child(row().grow(1.0))
            .child(
                Button::new("Reload")
                    .variant(ButtonVariant::Ghost)
                    .on_press(move |s: &mut State| s.reload(&id))
                    .build(k),
            ),
    )
}

/// The Session's diff, with each file's Comment and Viewed controls and its Threads under their
/// lines. The diff view finds its state by Session id, as it outlives this call.
fn session_diff(k: &Kit, s: &State, tab: &SessionTab, agent: Color) -> El {
    let id = tab.session.id.clone();
    let id_mut = id.clone();
    let id_header = id.clone();
    let kc = k.clone();
    DiffView::new(
        move |s: &State| &s.tabs.iter().find(|t| t.session.id == id).expect("the diff view's Session has a tab").diff,
        move |s: &mut State| {
            &mut s.tabs.iter_mut().find(|t| t.session.id == id_mut).expect("the diff view's Session has a tab").diff
        },
    )
    .key(format!("diff-{}", tab.session.id))
    .header_actions(move |s: &State, file: usize| {
        let tab = s.tabs.iter().find(|t| t.session.id == id_header).expect("the diff view's Session has a tab");
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
                    .on_toggle(move |s: &mut State| report(s.service.mark_viewed(&id, &path, !viewed)))
                    .build(&kc),
            )
    })
    .annotation({
        let k = k.clone();
        move |s: &State, a: &Annotation| annotation(&k, s, a, agent)
    })
    .on_comment(|s: &mut State, range: LineRange, _: &mut Tx| s.compose(range))
    .build(s, k)
}

/// The Threads on the Review as a whole and on whole files, which have no line in the diff,
/// and the composer when it's writing one.
fn general_threads(k: &Kit, s: &State, tab: &SessionTab, agent: Color) -> El {
    // Outdated File Threads show with the other Outdated Threads instead.
    let general = |t: &Thread| !t.outdated && !matches!(t.anchor, Anchor::Lines { .. });
    let threads: Vec<&Thread> = tab.session.threads.iter().filter(|t| general(t)).collect();
    let composing = s.composer.as_ref().is_some_and(|c| c.session == tab.session.id && c.at.is_none());
    if threads.is_empty() && !composing {
        return col();
    }
    let mut list = col().key("general-threads").w_full().px(16.0).py(8.0).gap(8.0);
    for thread in threads {
        list = list.child(thread_view(k, s, thread, &tab.session.id, agent));
    }
    if composing {
        list = list.child(composer(k, s));
    }
    col().key("general-scroll").w_full().max_h(320.0).scroll(ScrollSpec { stick_bottom: false }).child(list)
}

/// What shows under a line of the active Session's diff: the composer, or the Thread `a` names.
fn annotation(k: &Kit, s: &State, a: &Annotation, agent: Color) -> El {
    if a.id == COMPOSER {
        return composer(k, s);
    }
    let tab = s.active_tab();
    match tab.and_then(|t| t.session.threads.iter().find(|t| t.id.as_str() == a.id).map(|th| (t, th))) {
        Some((tab, thread)) => thread_view(k, s, thread, &tab.session.id, agent),
        None => col(),
    }
}

/// A Thread of `session`: its card, or one line while it's Resolved. A co-Reviewer's Draft the
/// human hasn't kept shows in the `agent` colour.
fn thread_view(k: &Kit, s: &State, thread: &Thread, session: &SessionId, agent: Color) -> El {
    if thread.resolved.is_some() {
        resolved_row(k, thread, session)
    } else if thread.comments.first().is_some_and(Comment::is_agent_draft) {
        agent_draft_card(k, s, thread, session, agent)
    } else {
        thread_card(k, thread, Some((s, session)))
    }
}

/// A co-Reviewer's Draft, dashed in the `agent` colour until the human keeps it, with Discard,
/// Question, Edit and Keep, and below it the questions about it and their answers. Nothing
/// reaches the Forge until it is kept.
fn agent_draft_card(k: &Kit, s: &State, thread: &Thread, session: &SessionId, agent: Color) -> El {
    let draft = &thread.comments[0];
    let mut card = card(k).border(1.0, agent).border_dash(4.0, 3.0).key(format!("agent-draft-{}", thread.id));
    card = card.child(
        row().gap(8.0).items_center().child(text("Agent Draft").font(k.ty.ui_medium).color(agent)).child(
            text(format!("{} · not kept yet · nothing reaches the Forge until you keep it", thread.id))
                .font(k.ty.small)
                .color(k.c.text_faint),
        ),
    );
    if let Some(label) = general_label(&thread.anchor) {
        card = card.child(text(label).font(k.ty.mono).color(k.c.text_muted));
    }
    let at = ThreadRef::new(session, &thread.id);
    if s.editing.as_ref() == Some(&at) {
        return card.child(edit_composer(k, s));
    }
    card = card.child(Markdown::new(draft.body.clone()).selectable(true).build(k));
    if let Some(suggestion) = &draft.suggestion {
        card = card.child(Markdown::new(suggestion_diff(&thread.excerpt, suggestion)).build(k));
    }
    card = comments(k, card, thread, &thread.comments[1..]);
    if s.reply.as_ref() == Some(&at) {
        return card.child(reply_composer(k, s));
    }
    let on = |action: fn(&Service, &ThreadRef) -> Result<(), akshually_core::Error>| {
        let at = at.clone();
        move |s: &mut State| s.act_on_thread(&at, action)
    };
    let (edit, question) = (at.clone(), at.clone());
    card.child(
        row()
            .w_full()
            .gap(8.0)
            .child(row().grow(1.0))
            .child(Button::new("Discard").variant(ButtonVariant::Ghost).on_press(on(Service::discard_draft)).build(k))
            .child(
                // A question is a reply to the Draft, which the next Round hands to the agent.
                Button::new("Question")
                    .variant(ButtonVariant::Ghost)
                    .on_press(move |s: &mut State| s.open_reply(question.clone()))
                    .build(k),
            )
            .child(
                Button::new("Edit")
                    .variant(ButtonVariant::Ghost)
                    .on_press(move |s: &mut State| s.open_edit(edit.clone()))
                    .build(k),
            )
            .child(Button::new("Keep").variant(ButtonVariant::Submit).on_press(on(Service::keep_draft)).build(k)),
    )
}

/// The editor of a co-Reviewer's Draft; saving keeps it as the human's.
fn edit_composer(k: &Kit, s: &State) -> El {
    col()
        .key("edit-composer")
        .w_full()
        .gap(8.0)
        .child(
            TextArea::new(&s.edit_text)
                .width(None)
                .auto_grow(2, 12)
                .on_edit(|s: &mut State| &mut s.edit_text)
                .on_submit(|s: &mut State, _| s.save_edit())
                .build(k),
        )
        .child(
            row()
                .w_full()
                .gap(8.0)
                .items_center()
                .child(text("Markdown · saving keeps it as your Draft").font(k.ty.small).color(k.c.text_faint))
                .child(row().grow(1.0))
                .child(
                    Button::new("Cancel")
                        .variant(ButtonVariant::Ghost)
                        .on_press(|s: &mut State| s.close_edit())
                        .build(k),
                )
                .child(
                    Button::new("Save and keep")
                        .variant(ButtonVariant::Submit)
                        .on_press(|s: &mut State| s.save_edit())
                        .build(k),
                ),
        )
}

/// A Draft as the markdown its author would have written: the body, then any suggestion fence.
fn draft_markdown(body: &str, suggestion: Option<&str>) -> String {
    let Some(code) = suggestion else { return body.to_owned() };
    let code = if code.ends_with('\n') || code.is_empty() { code.to_owned() } else { format!("{code}\n") };
    let fence = fence_for(&code);
    format!("{}\n\n{fence}suggestion\n{code}{fence}\n", body.trim_end())
}

/// Reopen for a Resolved Thread, Resolve for one that has reached the agent; nothing for a
/// Thread still all Drafts.
fn resolve_button(k: &Kit, thread: &Thread, session: &SessionId) -> Option<El> {
    let at = ThreadRef::new(session, &thread.id);
    if thread.resolved.is_some() {
        let reopen = move |s: &mut State| s.act_on_thread(&at, Service::reopen_thread);
        Some(Button::new("Reopen").variant(ButtonVariant::Ghost).on_press(reopen).build(k))
    } else if thread.comments.iter().any(|c| !c.is_draft()) {
        let resolve = move |s: &mut State| s.act_on_thread(&at, Service::resolve_thread);
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
fn resolved_row(k: &Kit, thread: &Thread, session: &SessionId) -> El {
    let first = first_line(thread);
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
fn thread_card(k: &Kit, thread: &Thread, actions: Option<(&State, &SessionId)>) -> El {
    let mut card = card(k);
    if let Some(label) = general_label(&thread.anchor) {
        card = card.child(text(label).font(k.ty.mono).color(k.c.text_muted));
    }
    card = comments(k, card, thread, &thread.comments);
    if thread.reopened {
        card = card.child(text("Reopened · goes out with your next Round").font(k.ty.small).color(k.c.text_faint));
    }
    // A Thread still all Drafts goes out whole with the next Round: nothing to reply to yet.
    let sent = thread.comments.iter().any(|c| !c.is_draft());
    if let Some((s, session)) = actions.filter(|_| sent) {
        let at = ThreadRef::new(session, &thread.id);
        card = if s.reply.as_ref() == Some(&at) {
            card.child(reply_composer(k, s))
        } else {
            card.child(
                row()
                    .w_full()
                    .gap(8.0)
                    .child(row().grow(1.0))
                    .child(
                        Button::new("Reply")
                            .variant(ButtonVariant::Ghost)
                            .on_press(move |s: &mut State| s.open_reply(at.clone()))
                            .build(k),
                    )
                    .children(resolve_button(k, thread, session)),
            )
        };
    }
    card
}

/// `comments` of `thread` added to `card`, each under who wrote it and when.
fn comments(k: &Kit, mut card: El, thread: &Thread, comments: &[Comment]) -> El {
    for comment in comments {
        let who = match &comment.author {
            Party::Human => "You",
            Party::Agent => "Agent",
            Party::Other(login) => login.as_str(),
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
    let fence = fence_for(&lines);
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
        let first = first_line(thread);
        let count = match thread.comments.len() {
            1 => "1 Comment".to_owned(),
            n => format!("{n} Comments"),
        };
        let at = ThreadRef::new(&tab.session.id, &thread.id);
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
            .children(resolve_button(k, thread, &tab.session.id))
            .child(
                Button::new(if expanded { "Hide" } else { "Show" })
                    .variant(ButtonVariant::Ghost)
                    .on_press(move |s: &mut State| s.toggle_outdated(&at))
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

/// The composer of a new Thread, with the Forge's refusal when its lines can't take a Comment.
fn composer(k: &Kit, s: &State) -> El {
    let error = s.composer.as_ref().and_then(|c| c.error.clone());
    // Lines the Forge refuses can still go on their file.
    let on_lines = s.composer.as_ref().is_some_and(|c| matches!(c.anchor, Anchor::Lines { .. }));
    card(k)
        .key("composer")
        .child(text(composer_title(s)).font(k.ty.small).color(k.c.text_faint))
        .children(error.map(|e| text(e).font(k.ty.small).color(k.c.danger.fg).wrap()))
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
                .children((on_lines && s.composer.as_ref().is_some_and(|c| c.error.is_some())).then(|| {
                    Button::new("Comment on the file")
                        .variant(ButtonVariant::Ghost)
                        .on_press(|s: &mut State| s.compose_on_file())
                        .build(k)
                }))
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

/// Why the Forge would refuse a Comment at `anchor` in `session`, if it would.
fn outside_diff(session: &SessionView, anchor: &Anchor) -> Option<String> {
    let Anchor::Lines { path, side, start, end } = anchor else { return None };
    let refused = session.pull_request().is_some() && !session.changeset.takes_comment_at(anchor);
    let (path, side, start, end) = (path.clone(), *side, *start, *end);
    refused.then(|| akshually_core::Error::OutsideDiff { path, side, start, end }.to_string())
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
        source: Source::Local { repo: "/home/me/akshually".into(), branch: "feat/round-snapshots".into() },
        agent: Some(Role::Author),
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
        pull_request_update: None,
    };
    let other = |id: &str, branch: &str, turn: Turn, attention: bool| SessionView {
        id: SessionId::new(id),
        source: Source::Local { repo: "/home/me/akshually".into(), branch: branch.into() },
        round: 1,
        threads: Vec::new(),
        turn,
        attention,
        ..session.clone()
    };
    let unseen = other("unseen", "fix/inbox-refresh", Turn::Human, true);
    let working = other("working", "feat/publish", Turn::Agent, false);
    s.apply(vec![session, unseen, working]);
    open_first_tab(s);
}

/// `--scene threads`: Review, file and suggestion Threads, with markdown bodies, an agent
/// reply, a human reply Draft and a Thread the agent resolved, under the banner offering files
/// changed on disk.
fn scene_threads(s: &mut State) {
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
        forge: None,
    };
    let session = SessionView {
        id: SessionId::new("sample"),
        source: Source::Local { repo: "/home/me/akshually".into(), branch: "feat/round-snapshots".into() },
        agent: Some(Role::Author),
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
        pull_request_update: None,
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
    open_first_tab(s);
}

/// Opens a scene's first Session (the scene's service has none, so nothing else would).
fn open_first_tab(s: &mut State) {
    if let Some(id) = s.tabs.first().map(|t| t.session.id.clone()) {
        s.open_tab(id);
    }
}

/// `--scene inbox`: the Inbox of design screen 3, with each row state.
fn scene_inbox(s: &mut State) {
    let now = now();
    let item = |repo: &str, number: u64, title: &str, author: &str, opened_h: i64, role| InboxItem {
        id: PullRequestRef { host: "github.com".into(), repo: repo.into(), number },
        kind: ForgeKind::GitHub,
        url: format!("https://github.com/{repo}/pull/{number}"),
        title: title.into(),
        author: author.into(),
        created_at: now - opened_h * 3600,
        updated_at: now - 600,
        role,
    };
    let rows = vec![
        (item("acme/web", 482, "Add SSO login", "maria", 2, InboxRole::Reviewer), InboxRowState::New),
        (
            InboxItem {
                id: PullRequestRef { host: "gitlab.example.com".into(), repo: "platform/infra".into(), number: 97 },
                kind: ForgeKind::GitLab,
                url: "https://gitlab.example.com/platform/infra/-/merge_requests/97".into(),
                ..item("platform/infra", 97, "Rotate database credentials", "tom", 24, InboxRole::Assignee)
            },
            InboxRowState::SessionOpen { session: SessionId::new("sample"), drafts: 2 },
        ),
        (
            item("acme/api", 311, "Retry 429 responses with backoff", "lee", 72, InboxRole::Reviewer),
            InboxRowState::Updated,
        ),
        (
            item("acme/docs", 45, "Fix broken links in setup guide", "sam", 120, InboxRole::Reviewer),
            InboxRowState::Seen,
        ),
    ];
    scene_session(s);
    s.open_inbox();
    s.inbox = InboxView {
        rows: rows.into_iter().map(|(item, state)| InboxRow { item, state }).collect(),
        refreshed_at: Some(now - 60),
        errors: Vec::new(),
        attention: false,
    };
}

/// `--scene publish`: a Pull Request Session with its Publish dialog open.
fn scene_publish(s: &mut State) {
    let pr = akshually_core::forge::PullRequest {
        id: PullRequestRef { host: "github.com".into(), repo: "acme/web".into(), number: 482 },
        url: "https://github.com/acme/web/pull/482".into(),
        title: "Add SSO login".into(),
        author: "maria".into(),
        head_branch: "sso".into(),
        head_sha: "head1".into(),
        base_sha: "base1".into(),
    };
    let mut suggested = sample_thread("T1", 3..=3, "Name it for what it holds.", None);
    suggested.comments[0].suggestion = Some("    let before = self.changeset.snapshot()?;\n".into());
    let mut reply = sample_thread("T2", 1..=1, "Agreed, the env var name is enough.", None);
    reply.anchor = Anchor::file("README.md");
    reply.forge = Some(akshually_core::ForgeThread { id: "PRRT_2".into(), resolved: false });
    let mut outdated = sample_thread("T4", 2..=2, "Typo in the doc comment.", None);
    outdated.outdated = true;
    let mut answered = sample_thread("T5", 4..=4, "Is the round number still needed?", None);
    answered.comments[0].author = Party::Other("lee".into());
    answered.forge = Some(akshually_core::ForgeThread { id: "PRRT_5".into(), resolved: false });
    let session = SessionView {
        id: SessionId::new("pr"),
        source: Source::PullRequest(pr),
        changeset: sample_changeset(),
        round: 1,
        threads: vec![suggested.clone(), sample_thread("T3", 5..=5, "Should this return the error as is?", None)],
        ..SessionView::default()
    };
    s.apply(vec![session]);
    let plan = PublishPlan {
        verdicts: Verdict::ALL.to_vec(),
        kept: vec![reply, suggested, sample_thread("T3", 5..=5, "Should this return the error as is?", None)],
        discarded: vec![outdated],
        resolve: vec![answered],
        reopen: Vec::new(),
    };
    s.publish = Some(Publish {
        session: SessionId::new("pr"),
        plan: Some(plan),
        verdict: Some(Verdict::RequestChanges),
        busy: false,
        error: None,
    });
}

/// `--scene co-review`: a Pull Request an agent co-reviews, with its Drafts not kept yet (one
/// with a suggestion, one on the Review) next to a Draft of the human's.
fn scene_co_review(s: &mut State) {
    scene_publish(s);
    s.publish = None;
    let Some(tab) = s.tabs.first() else { return };
    let mut session = tab.session.clone();
    session.agent = Some(Role::CoReviewer);
    for thread in &mut session.threads {
        thread.comments[0].author = Party::Agent;
    }
    let mut review = sample_thread("T4", 1..=1, "Small and focused; the snapshot is the only risk.", None);
    review.anchor = Anchor::Review;
    review.comments[0].author = Party::Agent;
    session.threads.push(review);
    session.threads.push(sample_thread("T5", 4..=4, "Is `Round::new` fallible now?", None));
    s.apply(vec![session]);
    open_first_tab(s);
}

fn sample_thread(id: &str, lines: std::ops::RangeInclusive<u32>, body: &str, round: Option<u32>) -> Thread {
    Thread {
        id: ThreadId::new(id),
        anchor: Anchor::lines(Side::New, "src/session.rs", lines),
        excerpt: String::new(),
        comments: vec![Comment { author: Party::Human, body: body.into(), round, suggestion: None }],
        resolved: None,
        outdated: false,
        reopened: false,
        forge: None,
    }
}

fn sample_changeset() -> Changeset {
    Changeset {
        base: "3e61ed7c0ffee".into(),
        files: vec![
            FileChange {
                binary: None,
                collapsed: None,
                hunks: None,
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
