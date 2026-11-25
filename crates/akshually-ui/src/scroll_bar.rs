//! The scroll bar over a Session's diff: a thumb on its right edge showing how far through the
//! Changeset the human is, which they can drag. Rune's `DiffView` builds its own scroll
//! viewport and takes no scroll bar, so the app draws one from the viewport's and the row
//! list's laid-out rects (ADR 0001 keeps Rune unpatched).

use rune::{Cx, El, Phase, Tx, el};
use rune_kit::Kit;

use crate::State;

/// How wide the thumb is, in points.
const THUMB_WIDTH: f32 = 8.0;

/// The thumb's gap from the diff's right edge, in points.
const EDGE: f32 = 2.0;

/// The thumb's gap from the top and bottom of the diff, in points.
const INSET: f32 = 2.0;

/// The shortest the thumb gets, however long the Changeset, so it stays grabbable.
const MIN_THUMB: f32 = 24.0;

/// How much taller than its viewport the content must be before the bar shows, in points:
/// the virtual row list's height is an estimate, and it must not flicker a bar on a diff that
/// just fits.
const OVERFLOW_SLACK: f32 = 1.0;

/// Where the thumb sits and what dragging it does, worked out from the diff's laid-out rects.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Thumb {
    /// The viewport's width.
    viewport_width: f32,
    /// The thumb's top and length within the viewport, in whole points so a view only
    /// rebuilds when the thumb has moved a pixel.
    top: f32,
    len: f32,
    /// How far the content scrolls, and how far the thumb travels over it: a drag of
    /// `travel` points scrolls the whole `scrollable` height.
    scrollable: f32,
    travel: f32,
    /// The content's current scroll offset.
    offset: f32,
}

impl Thumb {
    /// The thumb for a viewport and row list rects (`[x, y, w, h]`, in window points after
    /// scrolling), or `None` when the content fits.
    fn of(viewport: [f32; 4], rows: [f32; 4]) -> Option<Thumb> {
        let [_, viewport_y, viewport_width, viewport_h] = viewport;
        let content = rows[3];
        let scrollable = content - viewport_h;
        if scrollable <= OVERFLOW_SLACK {
            return None;
        }
        let track = (viewport_h - 2.0 * INSET).max(0.0);
        let len = (track * viewport_h / content).clamp(MIN_THUMB.min(track), track).round();
        let travel = track - len;
        let offset = (viewport_y - rows[1]).clamp(0.0, scrollable);
        let top = (INSET + travel * offset / scrollable).round();
        Some(Thumb { viewport_width, top, len, scrollable, travel, offset: offset.round() })
    }
}

/// The scroll bar over the diff keyed `diff_key` (the key its `DiffView` was given), to sit in
/// a sibling of the diff that has no other children: it is laid out over the diff's right
/// edge and takes none of its width. `None` while the diff has not been laid out or fits.
pub(crate) fn overlay(k: &Kit, cx: &mut Cx<State>, diff_key: &str) -> Option<El> {
    let scroll = format!("{diff_key}/scroll");
    let viewport = cx.window_layout(&scroll)?;
    // Whole points, so the view rebuilds when the thumb moves a pixel, not on every scrolled frame.
    let thumb = cx.window_layout_with(&format!("{scroll}/rows"), move |rows| Thumb::of(viewport, rows))??;
    let Thumb { viewport_width, top, len, scrollable, travel, offset } = thumb;
    let drag_scroll = scroll.clone();
    Some(
        el().key("diff-scroll-thumb")
            .absolute(viewport_width - THUMB_WIDTH - EDGE, top)
            .w(THUMB_WIDTH)
            .h(len)
            .radius(THUMB_WIDTH / 2.0)
            .fill(k.c.border_strong)
            .hover(k.c.text_weak)
            .on_drag_any(move |d, tx: &mut Tx| {
                if d.phase == Phase::Down {
                    d.data = offset;
                }
                if travel > 0.0 && d.phase != Phase::Up {
                    let y = d.data + d.dy * scrollable / travel;
                    tx.scroll_to_y(drag_scroll.clone(), y.clamp(0.0, scrollable));
                }
            }),
    )
}
