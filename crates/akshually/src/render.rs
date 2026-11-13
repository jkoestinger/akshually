//! What `review` prints: the CLI, not the app, formats the outcome, so stdout is defined here.

use std::fmt::Write;

use akshually_core::{Anchor, Outcome, Round, Thread, fence_for};

/// What `review --format markdown` prints for `outcome`; `pr` is the Pull Request the agent
/// co-reviews, if any, which its commands must name.
pub(crate) fn outcome_markdown(outcome: &Outcome, pr: Option<&str>) -> String {
    match outcome {
        Outcome::Round(round) => round_markdown(round, pr),
        Outcome::Approved => "Approved.\n".to_owned(),
        Outcome::Abandoned => {
            "Abandoned: the human closed this Session without a Verdict. Stop working on this change.\n".to_owned()
        }
        Outcome::Published { verdict, url } => format!("Published with the Verdict {verdict}: {url}\n"),
        Outcome::Closed => "Closed: the human finished with your co-review. There is nothing left to do.\n".to_owned(),
    }
}

/// A Round as markdown: one section per Thread, headed by its stable ID and Anchor, with the
/// anchored code and then each Comment's markdown body.
fn round_markdown(round: &Round, pr: Option<&str>) -> String {
    let count = round.threads.len();
    let pr = pr.map(|url| format!(" --pr {url}")).unwrap_or_default();
    let mut out = format!(
        "# Round {}\n\nThe human handed over {count} Thread{}. Address each one, answer it with \
         `akshually reply{pr} <thread> \"…\"` or `akshually resolve{pr} <thread>`, then run \
         `akshually review{pr}` again.\n",
        round.number,
        if count == 1 { "" } else { "s" },
    );
    for thread in &round.threads {
        thread_markdown(&mut out, thread);
    }
    out
}

fn thread_markdown(out: &mut String, thread: &Thread) {
    let outdated = if thread.outdated { " · Outdated" } else { "" };
    let reopened = if thread.reopened { " · Reopened" } else { "" };
    let _ = writeln!(out, "\n## {} · {}{outdated}{reopened}", thread.id, thread.anchor);
    if thread.reopened {
        out.push_str("\nThe human reopened this Thread: it isn't dealt with yet.\n");
    }
    if thread.outdated {
        out.push_str("\nThis code is no longer in the Changeset; the Thread was written against it:\n");
    }
    if let Anchor::Lines { .. } = thread.anchor {
        out.push('\n');
        code_block(out, &thread.excerpt);
    }
    for comment in &thread.comments {
        let _ = writeln!(out, "\n{}", comment.body.trim_end());
        if let (Some(code), Anchor::Lines { .. }) = (&comment.suggestion, &thread.anchor) {
            let _ = writeln!(out, "\nSuggested change to {}:\n", thread.anchor);
            code_block(out, code);
        }
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
