//! Search: finding text in the code of the Changeset a Session shows, and moving from one match
//! to the next. It looks in the lines of the diff, or in the open Rendered view's text, and
//! never in Threads.

use std::ops::Range;

use rune::{Key, KeyEvent, TextEdit, Tx, row, text};
use rune_kit::diff::{self, LineKind, LineRef};
use rune_kit::markdown::{self, Case};
use rune_kit::search::Fold;
use rune_kit::{Highlight, IconButton, IconButtonVariant, Kit, TextInput};

use akshually_core::{DiffLayout, FileChange, SessionId};

use crate::{SessionTab, State};

/// The key of the Rendered view's current match, which the view scrolls to.
const CURRENT_MATCH: &str = "search-current";

/// How wide the search field is, in points.
const FIELD_WIDTH: f32 = 280.0;

/// One tab's Search: the query, and which match the human is on. The UI's own, so it's lost on
/// restart; the matches themselves are found afresh from the Changeset each time they're
/// needed, so a reload or an expanded stretch never leaves them stale.
pub(crate) struct Search {
    /// The search field's text and focus. Rune's own focus is the truth for whether the field
    /// has it: ask [`State::search_has_focus`].
    query: TextEdit,
    /// The query the matches were last moved to, so a caret move in the field doesn't jump.
    last_query: String,
    /// The search field is showing.
    open: bool,
    /// Where the human is: the current match, by its place, so it survives the matches being
    /// found again.
    current: Option<(MatchPlace, usize)>,
}

impl Search {
    pub(crate) fn new() -> Self {
        Search { query: TextEdit::new(""), last_query: String::new(), open: false, current: None }
    }
}

/// Where a match is, ordered as the matches are stepped through.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum MatchPlace {
    /// A line of the diff of the `file`th file. `anchor` is the line's number on the new side,
    /// or for a removed line the new line that follows it, so a line keeps its place in the
    /// order when a hidden stretch above it is expanded.
    Line { file: usize, anchor: u32, side: diff::Side, line: u32 },
    /// The text of a column of the Rendered view: the old version is 0 when both show.
    Column(usize),
}

/// One match of the query.
pub(crate) struct Match {
    place: MatchPlace,
    /// Where it starts in its line, or in its Rendered column's source: it tells matches on one
    /// line apart.
    start: usize,
    /// The source bytes it covers, for highlighting in the Rendered view.
    ranges: Vec<Range<usize>>,
}

impl Match {
    fn key(&self) -> (MatchPlace, usize) {
        (self.place, self.start)
    }
}

/// The matches of `tab`'s query, in order: in the open Rendered view's text when it has one,
/// else in the diff. None while the search is closed or its query empty.
pub(crate) fn matches(tab: &SessionTab, layout: DiffLayout) -> Vec<Match> {
    let query = tab.search.query.text.as_str();
    if !tab.search.open || query.is_empty() {
        return Vec::new();
    }
    match &tab.rendered {
        Some(path) => rendered_matches(tab, layout, path, query),
        None => diff_matches(tab, query),
    }
}

/// The index in `found` of the match the human is on: the one they went to, or the next after
/// it when it's gone, or the first.
pub(crate) fn current(tab: &SessionTab, found: &[Match]) -> Option<usize> {
    if found.is_empty() {
        return None;
    }
    let at = tab.search.current.and_then(|c| found.iter().position(|m| m.key() >= c));
    Some(at.unwrap_or(0))
}

/// The highlights of the matches in the `column`th column of the Rendered view, the current one
/// distinct.
pub(crate) fn highlights(
    found: &[Match],
    current: Option<usize>,
    column: usize,
) -> Vec<(Vec<Range<usize>>, Highlight)> {
    let in_column = found.iter().enumerate().filter(|(_, m)| m.place == MatchPlace::Column(column));
    in_column
        .map(|(i, m)| {
            let highlight = match Some(i) == current {
                true => Highlight::current().key(CURRENT_MATCH),
                false => Highlight::new().key(format!("search-match-{i}")),
            };
            (m.ranges.clone(), highlight)
        })
        .collect()
}

/// The columns the Rendered view of `change` shows, in the order it shows them, each as the
/// version of the file it renders and its text. The index of a column in this list is the one
/// [`highlights`] takes and a match's `MatchPlace::Column` holds, so the view builds its columns from
/// it.
pub(crate) fn rendered_columns(change: &FileChange, layout: DiffLayout) -> Vec<(diff::Side, &str)> {
    use diff::Side::{New, Old};
    match (change.old.as_deref(), change.new.as_deref()) {
        (None, Some(new)) => vec![(New, new)],
        (Some(old), None) => vec![(Old, old)],
        (Some(old), Some(new)) => match layout {
            DiffLayout::SideBySide => vec![(Old, old), (New, new)],
            DiffLayout::Stacked => vec![(New, new)],
        },
        (None, None) => Vec::new(),
    }
}

fn rendered_matches(tab: &SessionTab, layout: DiffLayout, path: &str, query: &str) -> Vec<Match> {
    let Some(change) = tab.session.shown().files.iter().find(|f| f.path == path) else { return Vec::new() };
    let case = Case::Folded(Fold::default());
    let columns = rendered_columns(change, layout).into_iter().enumerate();
    columns
        .flat_map(|(column, (_, source))| {
            markdown::find(source, query, case).into_iter().filter_map(move |ranges| {
                let start = ranges.first()?.start;
                Some(Match { place: MatchPlace::Column(column), start, ranges })
            })
        })
        .collect()
}

fn diff_matches(tab: &SessionTab, query: &str) -> Vec<Match> {
    let fold = Fold::default();
    let mut found = Vec::new();
    for (file, change) in tab.session.shown().files.iter().enumerate() {
        let Some(diff) = tab.diff.files().get(file).filter(|_| change.binary.is_none()) else { continue };
        for (anchor, side, line, source) in searched_lines(tab, file, change, diff) {
            for range in fold.find(source, query) {
                let place = MatchPlace::Line { file, anchor, side, line };
                found.push(Match { place, start: range.start, ranges: vec![range] });
            }
        }
    }
    found
}

/// The first line number a hunk covers on a side that starts at `start` and has `count` lines:
/// a hunk with none starts at the line after.
fn first_line(start: u32, count: u32) -> u32 {
    if count == 0 { start.saturating_add(1) } else { start.max(1) }
}

/// The lines of `file`'s diff that Search looks in, as `(anchor, side, line, text)` (see
/// [`MatchPlace::Line`]): those of its hunks, and the unchanged stretches between them that the
/// human has expanded. A stretch before the first hunk or after the last that is expanded only
/// partly is searched as far as it was expanded, as it grows from its hunk. A middle stretch
/// expanded only partly is skipped, though some of its lines show: it can grow from either end,
/// and the diff view says how many of its lines are still hidden (`DiffState::hidden`) but not
/// which.
fn searched_lines<'a>(
    tab: &SessionTab,
    file: usize,
    change: &'a FileChange,
    diff: &'a diff::FileDiff,
) -> Vec<(u32, diff::Side, u32, &'a str)> {
    let mut lines = Vec::new();
    // An unchanged line is the same text on both sides, so it's found once, as the diff shows it.
    let (text, side) = match (&change.new, &change.old) {
        (Some(new), _) => (new.as_str(), diff::Side::New),
        (None, old) => (old.as_deref().unwrap_or(""), diff::Side::Old),
    };
    // Split only once a stretch turns out to be showing: most files never reveal one.
    let mut context: Option<Vec<&str>> = None;
    let old_total = change.old.as_deref().map_or(0, |old| old.lines().count() as u32);
    let (mut old_at, mut new_at) = (1u32, 1u32);
    for gap in 0..=diff.hunks.len() {
        let hunk = diff.hunks.get(gap);
        let len = match hunk {
            Some(h) => first_line(h.old_start, h.old_count).saturating_sub(old_at),
            None => (old_total + 1).saturating_sub(old_at),
        };
        let hidden = tab.diff.hidden(file, gap);
        let revealed = hidden.map_or(0, |hidden| len.saturating_sub(hidden));
        // From the hunk it grows from: the first stretch upwards from the first hunk, the last
        // downwards from the last, and a middle one only when it's whole.
        let shown = match (gap, hidden) {
            (0, _) => len - revealed..len,
            (_, Some(0)) => 0..len,
            _ if hunk.is_none() => 0..revealed,
            _ => 0..0,
        };
        for k in shown {
            let context = context.get_or_insert_with(|| text.lines().collect());
            let (anchor, line) = (new_at + k, if side == diff::Side::New { new_at + k } else { old_at + k });
            if let Some(&source) = context.get(line.saturating_sub(1) as usize) {
                lines.push((anchor, side, line, source));
            }
        }
        let Some(hunk) = hunk else { break };
        let mut following = first_line(hunk.new_start, hunk.new_count);
        for l in &hunk.lines {
            match (l.kind, l.old, l.new) {
                (LineKind::Removed, Some(old), _) => lines.push((following, diff::Side::Old, old, l.text.as_str())),
                (_, _, Some(new)) => {
                    lines.push((new, diff::Side::New, new, l.text.as_str()));
                    following = new + 1;
                }
                _ => {}
            }
        }
        old_at = first_line(hunk.old_start, hunk.old_count) + hunk.old_count;
        new_at = first_line(hunk.new_start, hunk.new_count) + hunk.new_count;
    }
    lines
}

/// Whether `ev` is Ctrl+F (Cmd+F on macOS), which opens the search field.
pub(crate) fn is_open_chord(ev: &KeyEvent) -> bool {
    ev.is_command('f') && !ev.mods.shift && !ev.mods.alt
}

/// The search field in a Session's header, below it: `None` while search is closed, or the
/// Rendered view is open and holds the field itself.
pub(crate) fn bar(k: &Kit, tab: &SessionTab, layout: DiffLayout) -> Option<rune::El> {
    if tab.rendered.is_some() {
        return None;
    }
    let found = matches(tab, layout);
    let field = field(k, tab, &found)?;
    Some(row().key("search-bar").w_full().px(16.0).py(8.0).child(field))
}

/// The search field, with its count of matches and its close button; `None` while search is
/// closed. `found` is `tab`'s matches.
pub(crate) fn field(k: &Kit, tab: &SessionTab, found: &[Match]) -> Option<rune::El> {
    if !tab.search.open {
        return None;
    }
    let (edit_id, change_id, key_id, close_id) =
        (tab.session.id.clone(), tab.session.id.clone(), tab.session.id.clone(), tab.session.id.clone());
    let input = TextInput::new(&tab.search.query)
        .placeholder("Search the code…")
        .width(Some(FIELD_WIDTH))
        .on_edit(move |s: &mut State| s.search_text(&edit_id))
        .on_change(move |s: &mut State, tx: &mut Tx| s.search_edited(&change_id, tx))
        .on_key(move |s: &mut State, ev: &KeyEvent, tx: &mut Tx| s.search_key(&key_id, ev, tx))
        .build(k)
        .key("search-field");
    let label = match (tab.search.query.text.is_empty(), current(tab, found)) {
        (true, _) => None,
        (false, None) => Some("No matches".to_owned()),
        (false, Some(at)) => Some(format!("{} of {}", at + 1, found.len())),
    };
    let close = IconButton::new(k.glyph.close)
        .variant(IconButtonVariant::Ghost)
        .label("Close search")
        .build(k)
        .key("search-close")
        .on_press(move |s: &mut State| {
            s.close_search(&close_id);
        });
    Some(
        row()
            .gap(12.0)
            .items_center()
            .child(input)
            .children(label.map(|n| text(n).font(k.ty.small).color(k.c.text_muted).key("search-count")))
            .child(close),
    )
}

impl State {
    /// Opens the search field of the displayed Session and focuses it. `false` on the Inbox,
    /// which has nothing to search.
    pub(crate) fn open_search(&mut self) -> bool {
        let Some(tab) = self.displayed_tab_mut() else { return false };
        tab.search.open = true;
        tab.search.query.focused = true;
        true
    }

    /// Closes `id`'s search field, giving the keys back to the diff. The query stays for when it
    /// opens again. `false` when it wasn't open.
    fn close_search(&mut self, id: &SessionId) -> bool {
        let Some(tab) = self.tab_mut(id).filter(|t| t.search.open) else { return false };
        tab.search.open = false;
        tab.search.query.focused = false;
        true
    }

    /// Closes the displayed Session's search field; `false` when it wasn't open.
    pub(crate) fn close_displayed_search(&mut self) -> bool {
        let Some(id) = self.displayed_tab().map(|t| t.session.id.clone()) else { return false };
        self.close_search(&id)
    }

    /// Whether the displayed Session's search field has keyboard focus, so a key is for the
    /// field and not for the diff behind it.
    pub(crate) fn search_has_focus(&self) -> bool {
        self.displayed_tab().is_some_and(|t| t.search.open && t.search.query.focused)
    }

    /// What `id`'s search field reads and writes. Once its tab is gone, a stale handler gets a
    /// field nothing shows, as `spare_diff` is for the diff view.
    fn search_text(&mut self, id: &SessionId) -> &mut TextEdit {
        // Not `tab_mut`: that would keep all of `self` borrowed in the `None` arm too.
        match self.tabs.iter_mut().find(|t| &t.session.id == id) {
            Some(tab) => &mut tab.search.query,
            None => &mut self.spare_search,
        }
    }

    /// The query of `id`'s search changed: goes to its first match. A caret move or selection
    /// in the field changes nothing.
    fn search_edited(&mut self, id: &SessionId, tx: &mut Tx) {
        let layout = self.diff_layout;
        let Some(tab) = self.tab_mut(id) else { return };
        if tab.search.query.text == tab.search.last_query {
            return;
        }
        tab.search.last_query = tab.search.query.text.clone();
        let found = matches(tab, layout);
        tab.search.current = found.first().map(Match::key);
        if let Some(first) = found.first() {
            go_to(tab, first, tx);
        }
    }

    /// Goes to the match after the current one of `id`'s search, or before it, wrapping round.
    fn step_search(&mut self, id: &SessionId, forward: bool, tx: &mut Tx) {
        let layout = self.diff_layout;
        let Some(tab) = self.tab_mut(id) else { return };
        let found = matches(tab, layout);
        let Some(at) = current(tab, &found) else { return };
        let n = found.len();
        let next = &found[if forward { (at + 1) % n } else { (at + n - 1) % n }];
        tab.search.current = Some(next.key());
        go_to(tab, next, tx);
    }

    /// The keys the search field takes before its own editing: Enter and Shift+Enter step
    /// through the matches, Escape closes it.
    fn search_key(&mut self, id: &SessionId, ev: &KeyEvent, tx: &mut Tx) -> bool {
        match ev.key {
            Key::Enter => {
                self.step_search(id, !ev.mods.shift, tx);
                true
            }
            Key::Escape => self.close_search(id),
            _ => false,
        }
    }
}

/// Brings `m` into view: its line in the diff, which unfolds its file if it's folded (the
/// viewed mark is the service's, and stays), or the Rendered view's current highlight.
fn go_to(tab: &mut SessionTab, m: &Match, tx: &mut Tx) {
    match m.place {
        MatchPlace::Line { file, side, line, .. } => tab.diff.scroll_to_line(file, LineRef::new(side, line), tx),
        MatchPlace::Column(_) => tx.scroll_center_in("rendered-scroll", CURRENT_MATCH),
    }
}
