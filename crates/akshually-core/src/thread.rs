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
    /// The whole of `path`.
    File { path: String },
    /// The Review as a whole.
    Review,
}

impl Anchor {
    /// The lines `range` (1-based, inclusive) of `path` on `side`.
    pub fn lines(side: Side, path: impl Into<String>, range: RangeInclusive<u32>) -> Self {
        Anchor::Lines { path: path.into(), side, start: *range.start(), end: *range.end() }
    }

    /// The whole of `path`.
    pub fn file(path: impl Into<String>) -> Self {
        Anchor::File { path: path.into() }
    }

    /// The anchored code in `changeset`, or why the Anchor doesn't fit it. Only line ranges
    /// hold code; other Anchors give an empty excerpt.
    pub(crate) fn excerpt(&self, changeset: &Changeset) -> Result<String, String> {
        let changed =
            |path: &str| changeset.files.iter().find(|f| f.path == path).ok_or_else(|| format!("{path} isn't changed"));
        let (path, side, start, end) = match self {
            Anchor::Lines { path, side, start, end } => (path, side, start, end),
            Anchor::File { path } => return changed(path).map(|_| String::new()),
            Anchor::Review => return Ok(String::new()),
        };
        let file = changed(path)?;
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
    /// The anchored code when the Thread was opened. Re-anchoring looks for it, and an
    /// Outdated Thread shows it.
    pub excerpt: String,
    pub comments: Vec<Comment>,
    /// Who marked it Resolved, while it is; either party can, and the human can reopen it.
    #[serde(default)]
    pub resolved: Option<Party>,
    /// Its Anchor no longer exists in the current Changeset; `anchor` is where it last was.
    #[serde(default)]
    pub outdated: bool,
    /// The human reopened it since the last Round, so the next Round tells the agent, even
    /// without a new Comment.
    #[serde(default)]
    pub reopened: bool,
}

/// Who wrote a Comment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Party {
    Human,
    Agent,
    /// Someone on the Forge, by login: a Comment that was on the Pull Request already.
    Other(String),
}

/// A single message inside a Thread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    pub author: Party,
    /// Markdown.
    pub body: String,
    /// The Round that carried it to the other side; `None` while it's a Draft, and for a
    /// Comment that was on the Forge already. An agent's reply, which the human sees at once,
    /// counts towards the Round being drafted.
    pub round: Option<u32>,
    /// Code proposed to replace the Thread's anchored lines (always new-side lines), whole.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

impl Comment {
    pub fn is_draft(&self) -> bool {
        self.round.is_none() && !matches!(self.author, Party::Other(_))
    }

    /// A co-Reviewer agent's Draft the human hasn't kept yet. Keeping it makes it the human's.
    pub fn is_agent_draft(&self) -> bool {
        self.round.is_none() && self.author == Party::Agent
    }
}

/// What a party writes to open a Thread: a markdown body, optionally with a code suggestion.
/// Plain text converts into one without a suggestion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewComment {
    pub body: String,
    pub suggestion: Option<String>,
}

impl NewComment {
    pub fn new(body: impl Into<String>) -> Self {
        NewComment { body: body.into(), suggestion: None }
    }

    /// Reads what someone wrote, taking the first closed ```` ```suggestion ```` fence (as on
    /// GitHub) out of the markdown as the suggestion.
    pub fn from_markdown(text: &str) -> Self {
        /// A backtick fence line's ticks and info string.
        fn fence_of(line: &str) -> Option<(&str, &str)> {
            let line = line.trim();
            let ticks = line.len() - line.trim_start_matches('`').len();
            (ticks >= 3).then(|| (&line[..ticks], line[ticks..].trim()))
        }
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        for (open, line) in lines.iter().enumerate() {
            let Some((ticks, "suggestion")) = fence_of(line) else { continue };
            let close = lines[open + 1..]
                .iter()
                .position(|l| fence_of(l).is_some_and(|(t, info)| t.len() >= ticks.len() && info.is_empty()));
            let Some(close) = close.map(|c| open + 1 + c) else { break };
            let (before, after) = (lines[..open].concat(), lines[close + 1..].concat());
            let parts = [before.trim_end(), after.trim_start()];
            let body = parts.iter().filter(|p| !p.is_empty()).copied().collect::<Vec<_>>().join("\n\n");
            return NewComment::new(body).suggest(lines[open + 1..close].concat());
        }
        NewComment::new(text)
    }

    /// Proposes `code` as the whole replacement for the anchored lines, which must be on the
    /// new side.
    pub fn suggest(mut self, code: impl Into<String>) -> Self {
        self.suggestion = Some(code.into());
        self
    }
}

impl From<&str> for NewComment {
    fn from(body: &str) -> Self {
        NewComment::new(body)
    }
}

impl From<String> for NewComment {
    fn from(body: String) -> Self {
        NewComment::new(body)
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
