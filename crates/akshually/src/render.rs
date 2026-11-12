//! What `review` prints: the CLI, not the app, formats the outcome, so stdout is defined here.

use std::fmt::Write;

use akshually_core::{Anchor, Round, Thread};

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
    let Anchor::Lines { path, side, start, end } = &thread.anchor;
    let lines = if start == end { format!("line {start}") } else { format!("lines {start}-{end}") };
    let _ = write!(out, "\n## {} · {path}, {side} {lines}\n\n", thread.id);
    let fence = fence_for(&thread.excerpt);
    let _ = write!(out, "{fence}\n{}", thread.excerpt);
    if !thread.excerpt.ends_with('\n') {
        out.push('\n');
    }
    let _ = writeln!(out, "{fence}");
    for comment in &thread.comments {
        let _ = writeln!(out, "\n{}", comment.body.trim_end());
    }
}

/// A backtick fence longer than any backtick run in `code`, so the code can't close it.
fn fence_for(code: &str) -> String {
    let longest = code.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    "`".repeat(longest.max(2) + 1)
}
