//! Re-anchoring: when a new Changeset arrives, each Thread follows its code by content, or
//! becomes Outdated when that code is gone.

use std::cmp::Reverse;

use crate::Changeset;
use crate::thread::{Anchor, Side, Thread};

/// Moves every Thread in `threads` to where its excerpt now is in `changeset`, or marks it
/// Outdated (keeping its last Anchor and its excerpt) when the code can't be found. An
/// Outdated Thread whose code reappears is placed again.
///
/// The excerpt is looked for on the Anchor's side of the Anchor's file: as is first, then
/// with small edits (more than half of its meaningful lines unchanged, ignoring
/// indentation). Among equal matches, the one nearest the Thread's last place wins.
pub fn reanchor(threads: &mut [Thread], changeset: &Changeset) {
    for thread in threads {
        match locate(thread, changeset) {
            Some(anchor) => {
                thread.anchor = anchor;
                thread.outdated = false;
            }
            None => thread.outdated = true,
        }
    }
}

fn locate(thread: &Thread, changeset: &Changeset) -> Option<Anchor> {
    let Anchor::Lines { path, side, start, .. } = &thread.anchor;
    let file = changeset.files.iter().find(|f| &f.path == path)?;
    let text = match side {
        Side::Old => file.old.as_deref(),
        Side::New => file.new.as_deref(),
    }?;
    let lines: Vec<&str> = text.lines().collect();
    let wanted: Vec<&str> = thread.excerpt.lines().collect();
    let n = wanted.len();
    if n == 0 || n > lines.len() {
        return None;
    }
    let distance = |at: usize| at.abs_diff(*start as usize - 1);
    let windows = 0..=lines.len() - n;

    let exact = windows.clone().filter(|&at| lines[at..at + n] == wanted[..]).min_by_key(|&at| distance(at));
    let at = exact.or_else(|| {
        let meaningful = wanted.iter().filter(|l| is_meaningful(l)).count();
        windows
            .map(|at| (similar_lines(&lines[at..at + n], &wanted), at))
            .filter(|&(same, _)| same * 2 > meaningful)
            .max_by_key(|&(same, at)| (same, Reverse(distance(at))))
            .map(|(_, at)| at)
    })?;
    Some(Anchor::Lines { path: path.clone(), side: *side, start: at as u32 + 1, end: (at + n) as u32 })
}

/// How many meaningful lines of `wanted` are in `window` at the same offset, ignoring
/// indentation and trailing whitespace.
fn similar_lines(window: &[&str], wanted: &[&str]) -> usize {
    window.iter().zip(wanted).filter(|(a, b)| is_meaningful(b) && a.trim() == b.trim()).count()
}

/// Blank lines and lone brackets match almost anywhere, so they don't count as evidence.
fn is_meaningful(line: &str) -> bool {
    line.chars().any(char::is_alphanumeric)
}
