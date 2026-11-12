//! Threads, the Comments inside them, their Anchors, and the Rounds that carry them between
//! the human and the agent.

use std::fmt;
use std::ops::RangeInclusive;

use serde::{Deserialize, Serialize};

use crate::Changeset;

/// Which side of a file's diff a line number counts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// The file at the Changeset's base.
    Old,
    /// The file as the agent left it.
    New,
}

/// Where a Thread is attached.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Anchor {
    /// Lines `start..=end` (1-based) of `path` on `side`.
    Lines { path: String, side: Side, start: u32, end: u32 },
}

impl Anchor {
    /// The lines `range` (1-based, inclusive) of `path` on `side`.
    pub fn lines(side: Side, path: impl Into<String>, range: RangeInclusive<u32>) -> Self {
        Anchor::Lines { path: path.into(), side, start: *range.start(), end: *range.end() }
    }

    /// The anchored code in `changeset`, or why the Anchor doesn't fit it.
    pub(crate) fn excerpt(&self, changeset: &Changeset) -> Result<String, String> {
        let Anchor::Lines { path, side, start, end } = self;
        let file = changeset.files.iter().find(|f| &f.path == path).ok_or_else(|| format!("{path} isn't changed"))?;
        let text = match side {
            Side::Old => &file.old,
            Side::New => &file.new,
        };
        let text = text.as_deref().ok_or_else(|| format!("{path} has no {side} side"))?;
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        if *start == 0 || start > end || *end as usize > lines.len() {
            return Err(format!("{path} has no {side} lines {start}-{end}"));
        }
        Ok(lines[*start as usize - 1..*end as usize].concat())
    }
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Side::Old => "old",
            Side::New => "new",
        })
    }
}

/// Names a Thread within its Session, stable across Rounds (`T1`, `T2`, …).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ThreadId(String);

impl ThreadId {
    pub fn new(id: impl Into<String>) -> Self {
        ThreadId(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ThreadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A conversation anchored to one place in a Changeset.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    pub id: ThreadId,
    pub anchor: Anchor,
    /// The anchored code when the Thread was opened.
    pub excerpt: String,
    pub comments: Vec<Comment>,
}

/// Who wrote a Comment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Party {
    Human,
    Agent,
}

/// A single message inside a Thread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    pub author: Party,
    /// Markdown.
    pub body: String,
    /// The Round that carried it to the other side; `None` while it's a Draft.
    pub round: Option<u32>,
}

impl Comment {
    pub fn is_draft(&self) -> bool {
        self.round.is_none()
    }
}

/// One batch of feedback handed over in one go: the Threads it touches, each holding only the
/// Comments this Round carries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Round {
    /// Counts up from 1 within the Session.
    pub number: u32,
    pub threads: Vec<Thread>,
}
