//! The Rendered view: a changed markdown file of a Changeset shown rendered over the diff, its
//! changed lines tinted. For reading only: it has no Threads.

use std::ops::Range;

use rune::{Color, El, ScrollSpec, col, el, row, text};
use rune_kit::diff::{self, LineKind};
use rune_kit::{Button, ButtonVariant, Highlight, IconButton, IconButtonVariant, Kit, Markdown};

use akshually_core::{DiffLayout, FileChange, SessionId};

use crate::{SessionTab, State, search, theme};

/// The margin around the Rendered view, between it and the window's edges, in points: it is
/// nearly as large as the window, with the diff still showing round it.
const RENDERED_MARGIN: f32 = 24.0;

impl State {
    /// Opens the Rendered view of `path` over the diff of `id`'s tab.
    pub(crate) fn open_rendered(&mut self, id: &SessionId, path: &str) {
        if let Some(tab) = self.tab_mut(id) {
            tab.rendered = Some(path.to_owned());
        }
    }

    /// Closes the Rendered view of `id`'s tab, leaving its diff as it was.
    fn close_rendered(&mut self, id: &SessionId) {
        if let Some(tab) = self.tab_mut(id) {
            tab.rendered = None;
        }
    }
}

/// The **Rendered** button in the header of a changed markdown text file, which opens its
/// Rendered view; `None` for any other file.
pub(crate) fn rendered_button(k: &Kit, id: &SessionId, change: &FileChange) -> Option<El> {
    if change.binary.is_some() || !is_markdown(&change.path) {
        return None;
    }
    let (id, path) = (id.clone(), change.path.clone());
    let key = format!("rendered-{path}");
    Some(
        Button::new("Rendered")
            .variant(ButtonVariant::GhostMuted)
            .on_press(move |s: &mut State| s.open_rendered(&id, &path))
            .build(k)
            .key(key),
    )
}

/// Whether `path` names a markdown file: it ends in `.md` or `.markdown`, in any case.
fn is_markdown(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    path.ends_with(".md") || path.ends_with(".markdown")
}

/// The Rendered view of `tab`'s open markdown file, over the diff: rendered side by side or
/// stacked as `layout` lays out the diff, with its changed lines tinted. Its own layer rather
/// than a `Dialog`, whose body is a fixed height or doesn't scroll. Nothing when it's closed or
/// its file isn't in the Changeset shown.
pub(crate) fn rendered_view(k: &Kit, dark: bool, layout: DiffLayout, tab: &SessionTab, layer: &mut Vec<El>) {
    let Some(path) = &tab.rendered else { return };
    let Some(change) = tab.session.shown().files.iter().find(|f| &f.path == path) else { return };
    let tokens = theme::tokens(dark);
    let (added, removed) = (tokens.color("diff-add-bg"), tokens.color("diff-del-bg"));
    let diff = tab.diff.file_index(path).and_then(|at| tab.diff.files().get(at));
    let changed = |kind: LineKind, side: diff::Side| -> Vec<u32> {
        let lines = diff.into_iter().flat_map(|d| &d.hunks).flat_map(|h| &h.lines);
        lines.filter(|l| l.kind == kind).filter_map(|l| l.number(side)).collect()
    };
    let found = search::matches(tab, layout);
    let current = search::current(tab, &found);
    let column = |at: usize, key: &str, text: &str, tint: Option<(&str, Color, Vec<u32>)>| {
        let mut items = Vec::new();
        if let Some((name, colour, lines)) = tint {
            let ranges = line_ranges(text, &lines).into_iter().enumerate();
            items.extend(
                ranges.map(|(i, range)| (vec![range], Highlight::new().bg(colour).key(format!("rendered-{name}-{i}")))),
            );
        }
        items.extend(search::highlights(&found, current, at));
        let markdown = Markdown::new(text).selectable(true).highlights(items);
        col().key(key).grow(1.0).min_w(0.0).child(markdown.build(k))
    };
    let mut removed_note = None;
    let columns = match (change.old.as_deref(), change.new.as_deref()) {
        (None, Some(new)) => vec![column(0, "rendered-new", new, None)],
        (Some(old), None) => vec![column(0, "rendered-old", old, None)],
        (Some(old), Some(new)) => {
            let at = match layout {
                DiffLayout::SideBySide => 1,
                DiffLayout::Stacked => 0,
            };
            let tint = Some(("added", added, changed(LineKind::Added, diff::Side::New)));
            let new = column(at, "rendered-new", new, tint);
            match layout {
                DiffLayout::SideBySide => {
                    let tint = ("removed", removed, changed(LineKind::Removed, diff::Side::Old));
                    vec![column(0, "rendered-old", old, Some(tint)), new]
                }
                DiffLayout::Stacked => {
                    removed_note = match changed(LineKind::Removed, diff::Side::Old).len() {
                        0 => None,
                        n => Some(format!("{}, see the diff", counted(n, "line removed", "lines removed"))),
                    };
                    vec![new]
                }
            }
        }
        (None, None) => return,
    };
    let open = match tab.session.open_threads_on(path) {
        0 => None,
        n => Some(counted(n, "open Thread", "open Threads")),
    };
    let (close_id, dismiss_id) = (tab.session.id.clone(), tab.session.id.clone());
    let close = IconButton::new(k.glyph.close)
        .variant(IconButtonVariant::Ghost)
        .label("Close")
        .build(k)
        .key("rendered-close")
        .on_press(move |s: &mut State| s.close_rendered(&close_id));
    let header = row()
        .key("rendered-header")
        .w_full()
        .padding(16.0)
        .gap(12.0)
        .items_center()
        .border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_weak)
        .child(text(path.clone()).font(k.ty.mono).color(k.c.text_base))
        .children(open.map(|n| text(n).font(k.ty.small).color(k.c.text_muted).key("rendered-threads")))
        .children(removed_note.map(|n| text(n).font(k.ty.small).color(k.c.text_muted).key("rendered-removed-note")))
        .child(row().grow(1.0))
        .children(search::field(k, tab, &found))
        .child(close);
    let body = col()
        .key("rendered-scroll")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .scroll(ScrollSpec { stick_bottom: false })
        .child(row().w_full().padding(16.0).gap(24.0).items_start().children(columns));
    let panel = col()
        .key("rendered-view")
        .role("dialog")
        .label(path.clone())
        .w_full()
        .h_full()
        .radius(6.0)
        .fill(k.c.layer_01)
        .child(header)
        .child(body)
        .modal(true, move |s: &mut State| s.close_rendered(&dismiss_id));
    layer.push(el().key("rendered-scrim").absolute(0.0, 0.0).w_full().h_full().fill(k.c.scrim).a11y_hidden());
    layer.push(el().key("rendered-layer").absolute(0.0, 0.0).w_full().h_full().padding(RENDERED_MARGIN).child(panel));
}

/// `n` of something: `1 open Thread`, `3 open Threads`.
fn counted(n: usize, one: &str, many: &str) -> String {
    if n == 1 { format!("1 {one}") } else { format!("{n} {many}") }
}

/// The byte ranges in `text` of its 1-based `lines`, each whole line without its line break.
/// Blank lines, which have nothing to tint, and lines past the end are left out.
fn line_ranges(text: &str, lines: &[u32]) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        ranges.push(at..at + line.trim_end_matches(['\n', '\r']).len());
        at += line.len();
    }
    let line = |n: u32| ranges.get((n as usize).checked_sub(1)?).cloned();
    lines.iter().filter_map(|&n| line(n)).filter(|range| !range.is_empty()).collect()
}
