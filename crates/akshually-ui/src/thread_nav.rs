//! Going from one unresolved Thread of the diff to the next: the `n` and `p` keys, and the two
//! buttons beside the Diff layout control. Only Threads under a line of the Changeset the
//! Session shows are visited, in the order the diff shows them. Which Threads are unresolved is
//! the core's call (`SessionView::unresolved_placed_threads`); this module orders them and
//! scrolls.

use std::cell::Cell;
use std::rc::Rc;

use rune::{KeyEvent, Tx};
use rune_kit::diff::{self, DiffState, LineRef};
use rune_kit::{IconButton, IconButtonVariant, Kit};

use akshually_core::SessionId;

use crate::{SessionTab, State, diff_key, diff_side};

/// The order of a row or a Thread in the diff, as the diff shows it: the file, the hunk in
/// it (the unchanged range before a hunk counts as the hunk's), and the line in the hunk, `1`
/// for its first line and `0` for what comes before its lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RowOrder {
    file: usize,
    hunk: usize,
    slot: usize,
}

impl RowOrder {
    /// The start of `file`.
    fn start_of(file: usize) -> RowOrder {
        RowOrder { file, hunk: 0, slot: 0 }
    }

    /// After every row of the diff.
    const END: RowOrder = RowOrder { file: usize::MAX, hunk: usize::MAX, slot: usize::MAX };
}

/// A Thread the keys visit.
struct Stop {
    order: RowOrder,
    /// The line it is under, to scroll to.
    line: LineRef,
}

/// How tall the sticky file header over the diff's top edge is, in points: Rune's diff view
/// keeps its own constant private. The row the view starts at is the one under it.
const STICKY_HEADER_HEIGHT: f32 = 44.0;

/// The unresolved Threads of `tab` under a line of its diff, in the order the diff shows them.
/// A Thread on a line the diff of the Changeset shown doesn't have is skipped.
fn stops(tab: &SessionTab) -> Vec<Stop> {
    let mut stops: Vec<Stop> = tab
        .session
        .unresolved_placed_threads()
        .into_iter()
        .filter_map(|p| {
            let side = diff_side(p.side);
            let file = tab.diff.files().get(p.file)?;
            let order = locate(p.file, file, side, p.line);
            // A line in an unchanged range the human hasn't expanded has no row to go to.
            (order.slot > 0).then_some(Stop { order, line: LineRef::new(side, p.line) })
        })
        .collect();
    // Stable: Threads on one line keep the order the service lists them in.
    stops.sort_by_key(|s| s.order);
    stops
}

/// Where line `number` of `side` is in `file`, the file at `at`: in the hunk that has it, or
/// before the first hunk after it if it is in an unchanged range between hunks.
fn locate(at: usize, file: &diff::FileDiff, side: diff::Side, number: u32) -> RowOrder {
    for (hunk, h) in file.hunks.iter().enumerate() {
        if let Some(line) = h.lines.iter().position(|l| l.number(side) == Some(number)) {
            return RowOrder { file: at, hunk, slot: line + 1 };
        }
    }
    let before = file.hunks.partition_point(|h| end(h, side) <= number);
    RowOrder { file: at, hunk: before, slot: 0 }
}

/// One past the last line number `hunk` covers on `side`.
fn end(hunk: &diff::Hunk, side: diff::Side) -> u32 {
    let (start, count) = match side {
        diff::Side::Old => (hunk.old_start, hunk.old_count),
        diff::Side::New => (hunk.new_start, hunk.new_count),
    };
    let first = if count == 0 { start.saturating_add(1) } else { start.max(1) };
    first.saturating_add(count)
}

/// Where the diff row whose element sits at `path` is, as far as it can be told: the row's key
/// is the first of the path's parts that names one (`df0.n95`: file 0, new line 95; `df0.g1`:
/// the unchanged range before hunk 1; `df0`: file 0's header). A side-by-side row pairing a
/// removed and an added line goes by the removed one.
fn row_order(diff: &DiffState, path: &str) -> Option<RowOrder> {
    let key = path
        .split('/')
        .find_map(|part| part.strip_prefix("df").filter(|r| r.starts_with(|c: char| c.is_ascii_digit())))?;
    let (file, tail) = key.split_once('.').map_or((key, None), |(f, t)| (f, Some(t)));
    let at: usize = file.parse().ok()?;
    let Some(tail) = tail.filter(|t| *t != "note") else { return Some(RowOrder::start_of(at)) };
    let (kind, number) = tail.split_at_checked(1)?;
    let number: u32 = number.parse().ok()?;
    let file = diff.files().get(at)?;
    match kind {
        "g" => Some(RowOrder { file: at, hunk: usize::try_from(number).ok()?, slot: 0 }),
        "c" | "o" => Some(locate(at, file, diff::Side::Old, number)),
        "n" => Some(locate(at, file, diff::Side::New, number)),
        _ => None,
    }
}

/// Whether `tab` has a Thread to visit.
fn any(tab: &SessionTab) -> bool {
    !stops(tab).is_empty()
}

/// Scrolls `id`'s diff to the next unresolved Thread below where it is scrolled, or the
/// previous one above it, wrapping round. Unfolds its file if folded, leaving the viewed mark.
/// Does nothing under a Rendered view, or with no Thread to visit.
///
/// Where the diff is scrolled is read from what the last frame drew: the row under the sticky
/// header at the top of the view, and the one at its bottom (see [`go`]).
fn visit(s: &mut State, id: &SessionId, forward: bool, tx: &mut Tx) {
    let Some(tab) = s.tab(id).filter(|t| t.rendered.is_none()) else { return };
    let Some([x, y, width, height]) = tab.viewport else {
        // Not laid out yet: from the start of the file the diff says is at the top.
        if let Some(tab) = s.tab_mut(id) {
            go(tab, forward, None, tx);
        }
        return;
    };
    let scope = format!("{}/scroll", diff_key(id));
    let x = x + width / 2.0;
    let top = Rc::new(Cell::new(None));
    let top_read = top.clone();
    let id_top = id.clone();
    tx.element_at_in(scope.clone(), x, y + STICKY_HEADER_HEIGHT + 1.0, move |s: &mut State, picked, _: &mut Tx| {
        let found = picked.zip(s.tab(&id_top)).and_then(|(p, tab)| row_order(&tab.diff, p.path()));
        top_read.set(found);
    });
    let id_bottom = id.clone();
    // The picks run one after the other as the same input is handled, so `top` is set by now.
    tx.element_at_in(scope, x, y + height - 1.0, move |s: &mut State, picked, tx: &mut Tx| {
        let Some(tab) = s.tab_mut(&id_bottom) else { return };
        // Below the last row the view is empty: everything after the top is in view.
        let bottom = picked.and_then(|p| row_order(&tab.diff, p.path())).unwrap_or(RowOrder::END);
        let view = top.get().map(|top| (top, bottom));
        go(tab, forward, view, tx);
    });
}

/// How many lines below the top of the view a Thread just gone to can be: the diff puts it a
/// few rows under its sticky header, and a row is a line, or two when side by side.
const LANDING_SLACK: usize = 6;

/// Scrolls `tab`'s diff to the Thread after (before) the one reached last when the view is still
/// where going there left it, and else after (before) the top of the view. `view` is the order
/// of the rows at the top and at the bottom of the view; with none, it is taken to start at the
/// file the diff says is at the top.
///
/// The Thread reached last is remembered only to tell, when several are in view at once, which
/// one the view is at. It counts only while it is still within `LANDING_SLACK` lines of the top
/// of the view, or the view ends the diff, where scrolling to it could not bring it higher: a
/// scroll by hand, or a reloaded Changeset, drops it, and the keys go by the view.
fn go(tab: &mut SessionTab, forward: bool, view: Option<(RowOrder, RowOrder)>, tx: &mut Tx) {
    let stops = stops(tab);
    let (top, bottom) = view.unwrap_or((RowOrder::start_of(tab.diff.visible_file()), RowOrder::END));
    let at_landing = |r: &RowOrder| {
        let Some(file) = tab.diff.files().get(r.file).filter(|_| r.file == top.file && *r >= top) else { return false };
        rank(file, *r) - rank(file, top) <= LANDING_SLACK
    };
    let at_end = bottom >= diff_end(&tab.diff);
    let reached = tab.reached.filter(|r| at_landing(r) || (at_end && (top..=bottom).contains(r)));
    let target = match (forward, reached) {
        (true, Some(from)) => stops.iter().find(|s| s.order > from),
        (true, None) => stops.iter().find(|s| s.order >= top),
        (false, from) => stops.iter().rev().find(|s| s.order < from.unwrap_or(top)),
    };
    let Some(stop) = target.or(if forward { stops.first() } else { stops.last() }) else { return };
    tab.reached = Some(stop.order);
    tab.diff.scroll_to_line(stop.order.file, stop.line, tx);
}

/// How many rows of `file` come before `order` in it, give or take the hunks' unchanged ranges
/// each counting as one.
fn rank(file: &diff::FileDiff, order: RowOrder) -> usize {
    let before: usize = file.hunks.iter().take(order.hunk).map(|h| h.lines.len() + 1).sum();
    before + order.slot
}

/// The order of the last line of the diff, the last row in the view's reach.
fn diff_end(diff: &DiffState) -> RowOrder {
    let Some((file, last)) = diff.files().iter().enumerate().next_back() else { return RowOrder::start_of(0) };
    let hunk = last.hunks.len().saturating_sub(1);
    RowOrder { file, hunk, slot: last.hunks.get(hunk).map_or(0, |h| h.lines.len()) }
}

/// The two buttons that go to the previous and the next unresolved Thread, for `id`'s header.
pub(crate) fn buttons(k: &Kit, tab: &SessionTab) -> Vec<rune::El> {
    let enabled = any(tab);
    let button = |glyph, key: &'static str, label: &str, forward: bool| {
        let id: SessionId = tab.session.id.clone();
        IconButton::new(glyph)
            .variant(IconButtonVariant::Ghost)
            .label(label)
            .help(format!("{label}  {}", if forward { "n" } else { "p" }))
            .disabled(!enabled)
            .on_press_tx(move |s: &mut State, tx: &mut Tx| visit(s, &id, forward, tx))
            .build(k)
            .key(key)
    };
    vec![
        button(k.glyph.chevron_up, "previous-thread", "Previous unresolved Thread", false),
        button(k.glyph.chevron_down, "next-thread", "Next unresolved Thread", true),
    ]
}

/// `n` and `p` for the displayed Session: the next and the previous unresolved Thread. Not under
/// a Rendered view, which covers the diff. A text field with focus takes a text key before the
/// window's key handler sees it, so no check for one is needed.
pub(crate) fn on_key(s: &mut State, ev: &KeyEvent, tx: &mut Tx) -> bool {
    let plain = !ev.mods.cmd && !ev.mods.ctrl && !ev.mods.alt;
    let forward = match &ev.key {
        rune::Key::Text(t) if plain && t == "n" => true,
        rune::Key::Text(t) if plain && t == "p" => false,
        _ => return false,
    };
    let Some(id) = s.displayed_tab().filter(|t| t.rendered.is_none()).map(|t| t.session.id.clone()) else {
        return false;
    };
    visit(s, &id, forward, tx);
    true
}
