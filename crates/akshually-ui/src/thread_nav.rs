//! Going from one unresolved Thread of the diff to the next: the `n` and `p` keys, and the two
//! buttons beside the Diff layout control. Only Threads under a line of the Changeset the
//! Session shows are visited, in the order the diff shows them.

use rune::{KeyEvent, Tx};
use rune_kit::diff::{self, LineRef};
use rune_kit::{IconButton, IconButtonVariant, Kit};

use akshually_core::SessionId;

use crate::{SessionTab, State, diff_side};

/// Where a Thread sits in the diff, ordered as the diff shows it: the file, the hunk in it, and
/// the line in the hunk.
type Place = (usize, usize, usize);

/// A Thread the keys visit.
struct Stop {
    place: Place,
    /// The line it is under, to scroll to.
    line: LineRef,
}

/// Where the human last went to with `n` or `p`, so the next press carries on from there.
/// `DiffState` doesn't say which line is under its sticky header, only which file is (see
/// [`start`]), so a Thread reached is the best record of where the view is. It is only trusted
/// while that file is still the one at the top, and dropped when the diff's own keys move.
#[derive(Clone, Copy)]
pub(crate) struct Reached(Place);

/// The unresolved Threads of `tab` under a line of its diff, in the order the diff shows them.
/// A Draft counts. Outdated Threads and General Threads have no line, and a Thread on a line the
/// diff of the Changeset shown doesn't have is skipped.
fn stops(tab: &SessionTab) -> Vec<Stop> {
    let open = |id: &str| tab.session.threads.iter().any(|t| t.id.as_str() == id && t.resolved.is_none());
    let mut stops: Vec<Stop> = tab
        .session
        .placed_threads()
        .into_iter()
        .filter(|p| open(p.thread.as_str()))
        .filter_map(|p| {
            let side = diff_side(p.side);
            let hunks = &tab.diff.files().get(p.file)?.hunks;
            let at = |l: &diff::Line| if side == diff::Side::New { l.new } else { l.old };
            let (hunk, line) = hunks
                .iter()
                .enumerate()
                .find_map(|(h, hunk)| hunk.lines.iter().position(|l| at(l) == Some(p.line)).map(|i| (h, i)))?;
            Some(Stop { place: (p.file, hunk, line), line: LineRef::new(side, p.line) })
        })
        .collect();
    // Stable: Threads on one line keep the order the service lists them in.
    stops.sort_by_key(|s| s.place);
    stops
}

/// Where the view is scrolled, as far as it can be told: the Thread last reached while the view
/// is still in its file, else the top of the file under the sticky header. Approximate: a
/// wheel scroll within a file that has a Thread reached goes unnoticed.
fn start(tab: &SessionTab, reached: Option<Reached>) -> (Place, bool) {
    let file = tab.diff.visible_file();
    match reached {
        Some(Reached(place)) if place.0 == file => (place, true),
        _ => ((file, 0, 0), false),
    }
}

/// Whether `tab` has a Thread to visit.
pub(crate) fn any(tab: &SessionTab) -> bool {
    !stops(tab).is_empty()
}

/// Scrolls to the next unresolved Thread of `tab`'s diff below where the view is, or the
/// previous one above it, wrapping round. Unfolds its file if folded, leaving the viewed mark.
/// Does nothing under a Rendered view, or with no Thread to visit.
pub(crate) fn visit(tab: &mut SessionTab, forward: bool, tx: &mut Tx) {
    if tab.rendered.is_some() {
        return;
    }
    let stops = stops(tab);
    let (from, exact) = start(tab, tab.reached);
    // From a Thread already reached, those after it; from the top of a file, those in the file
    // too (the first of them is below the header), and those before it are the ones above.
    let below = |s: &&Stop| if exact { s.place > from } else { s.place >= from };
    let above = |s: &&Stop| s.place < from;
    let target = if forward {
        stops.iter().find(below).or(stops.first())
    } else {
        stops.iter().rev().find(above).or(stops.last())
    };
    let Some(stop) = target else { return };
    tab.reached = Some(Reached(stop.place));
    tab.diff.scroll_to_line(stop.place.0, stop.line, tx);
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
            .on_press_tx(move |s: &mut State, tx: &mut Tx| s.visit_thread(&id, forward, tx))
            .build(k)
            .key(key)
    };
    vec![
        button(k.glyph.chevron_up, "previous-thread", "Previous unresolved Thread", false),
        button(k.glyph.chevron_down, "next-thread", "Next unresolved Thread", true),
    ]
}

impl State {
    /// Goes to the next or previous unresolved Thread of `id`'s Session; see [`visit`].
    fn visit_thread(&mut self, id: &SessionId, forward: bool, tx: &mut Tx) {
        if let Some(tab) = self.tab_mut(id) {
            visit(tab, forward, tx);
        }
    }
}

/// The diff's keys for the displayed Session, with `n` and `p` among them. Not under a Rendered
/// view, which covers the diff, nor while a text field has focus.
pub(crate) fn on_diff_key(s: &mut State, ev: &KeyEvent, tx: &mut Tx) -> bool {
    let Some(tab) = s.displayed_tab_mut().filter(|t| t.rendered.is_none()) else { return false };
    if tab.diff.key(ev, tx) {
        // The diff moved on its own: where the last Thread was reached no longer says where it is.
        tab.reached = None;
        return true;
    }
    let plain = !ev.mods.cmd && !ev.mods.ctrl && !ev.mods.alt;
    let forward = match &ev.key {
        _ if tx.focused().is_some() || !plain => return false,
        rune::Key::Text(t) if t == "n" => true,
        rune::Key::Text(t) if t == "p" => false,
        _ => return false,
    };
    visit(tab, forward, tx);
    true
}
