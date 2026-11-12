//! What `review` prints: the CLI, not the app, formats the outcome, so stdout is defined here.

use std::fmt::Write;

use akshually_core::{Anchor, Round, Side, Thread};

/// A Round as markdown: one section per Thread, headed by its stable ID and Anchor, with the
/// anchored code and then each Comment's markdown body.
pub fn round_markdown(round: &Round) -> String {
    let count = round.threads.len();
    let mut out = format!(
        "# Round {}\n\nThe human handed over {count} Thread{}. Address each one, then run `akshually review` again.\n",
        round.number,
        if count == 1 { "" } else { "s" },
    );
    for thread in &round.threads {
        thread_markdown(&mut out, thread);
    }
    out
}

fn thread_markdown(out: &mut String, thread: &Thread) {
    let _ = writeln!(out, "\n## {} · {}", thread.id, anchor_markdown(&thread.anchor));
    if let Anchor::Lines { .. } = thread.anchor {
        out.push('\n');
        code_block(out, &thread.excerpt);
    }
    for comment in &thread.comments {
        let _ = writeln!(out, "\n{}", comment.body.trim_end());
        if let (Some(code), Anchor::Lines { path, side, start, end }) = (&comment.suggestion, &thread.anchor) {
            let _ = writeln!(out, "\nSuggested change: replace {} of {path} with\n", lines(*side, *start, *end));
            code_block(out, code);
        }
    }
}

/// `new lines 2-3`, or `old line 1`.
fn lines(side: Side, start: u32, end: u32) -> String {
    if start == end { format!("{side} line {start}") } else { format!("{side} lines {start}-{end}") }
}

/// Where a Thread sits, naming its Anchor kind: `lib.rs, new lines 2-3`, `lib.rs, whole file`
/// or `whole Review`.
fn anchor_markdown(anchor: &Anchor) -> String {
    match anchor {
        Anchor::Lines { path, side, start, end } => format!("{path}, {}", lines(*side, *start, *end)),
        Anchor::File { path } => format!("{path}, whole file"),
        Anchor::Review => "whole Review".to_owned(),
    }
}

/// `code` in a fence it can't close.
fn code_block(out: &mut String, code: &str) {
    let fence = fence_for(code);
    let _ = write!(out, "{fence}\n{code}");
    if !code.ends_with('\n') {
        out.push('\n');
    }
    let _ = writeln!(out, "{fence}");
}

/// A backtick fence longer than any backtick run in `code`, so the code can't close it.
fn fence_for(code: &str) -> String {
    let longest = code.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    "`".repeat(longest.max(2) + 1)
}
