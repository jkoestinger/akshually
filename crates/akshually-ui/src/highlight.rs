//! Syntax colours of a Changeset's diff, taken from the whole text of each side of a file.
//!
//! Rune's diff view highlights a file 128 lines at a time, each chunk as if it were a whole
//! source, so a string or comment that crosses a chunk boundary loses its context (#106). The
//! `Highlighter` here knows the texts of the Changeset shown, finds where a chunk sits in them,
//! and answers with that part of the whole text's highlights when every place it sits agrees.

use std::cell::OnceCell;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::ops::Range;

use rune_kit::syntax::{self, Highlight, Highlighter};

use akshually_core::Changeset;

/// How many bytes of a text Rune's built-in highlighter colours (its `MAX_SOURCE`).
const COLOURED_BYTES: usize = 128 * 1024;

/// How many highlights Rune's built-in highlighter makes of a text (its `MAX_TOKENS`).
const COLOURED_RANGES: usize = 32 * 1024;

/// The highlights of a whole text, and how far they can be trusted.
struct Whole {
    highlights: Vec<Highlight>,
    /// Up to where the text is really coloured: past it, a range is plain only because Rune gave up.
    coloured: usize,
}

/// One side of a changed file.
struct Text {
    /// The highlighter language of the file's path.
    lang: Option<&'static str>,
    /// The text as Rune chunks it: see [`as_chunked`].
    text: String,
    /// The highlights of the whole `text`, made when a chunk of it is first asked for.
    whole: OnceCell<Whole>,
}

impl Text {
    fn whole(&self) -> &Whole {
        self.whole.get_or_init(|| {
            let highlights = syntax::builtin(self.lang, &self.text);
            // Rune stops at the byte limit or at the range limit and says neither. Either way the
            // highlights end early, and what lies past the last of them was never looked at. Hitting
            // the range limit exactly is taken as cut, which only costs a per-chunk answer.
            let cut = self.text.len() > COLOURED_BYTES || highlights.len() >= COLOURED_RANGES;
            let coloured = if cut { highlights.last().map_or(0, |(r, _)| r.end) } else { self.text.len() };
            Whole { highlights, coloured }
        })
    }

    /// Every place in this text where `lines` (whole lines, none empty at either end) can be the
    /// lines of a chunk that ends in `blank` empty lines: they follow `lines` in the text, or
    /// the text ends in empty lines and the rest of them is the chunk's padding.
    fn find_all(&self, lines: &str, blank: usize) -> Vec<Range<usize>> {
        let text = self.text.as_str();
        let mut found = Vec::new();
        let mut from = 0;
        while let Some(at) = text[from..].find(lines).map(|i| from + i) {
            let end = at + lines.len();
            // The next match to look at starts on a later line, so overlapping ones count.
            from = at + lines.find('\n').map_or(lines.len(), |i| i + 1);
            let at_line_start = at == 0 || text.as_bytes()[at - 1] == b'\n';
            let ends_the_line = end == text.len() || text.as_bytes()[end] == b'\n';
            let after = text[end..].strip_prefix('\n').unwrap_or("");
            let empty = after.bytes().take_while(|&b| b == b'\n').count();
            if at_line_start && ends_the_line && (empty >= blank || after[empty..].is_empty()) {
                found.push(at..end);
            }
        }
        found
    }
}

/// `text` as Rune chunks it: Rune cuts lines at `\n` and drops the `\r` before it, and one that
/// ends the text, so the chunks of a file with `\r\n` line ends hold `\n` alone.
fn as_chunked(text: &str) -> String {
    let mut text = text.replace("\r\n", "\n");
    if text.ends_with('\r') {
        text.pop();
    }
    text
}

/// The `Highlighter` for the diff of `changeset`: every line gets the colours it has when the whole
/// text of its side is highlighted in one go, wherever it sits in the file.
///
/// A chunk that is in no text of `changeset`, or in none of its language, is highlighted on its
/// own, as Rune's built-in highlighter would. So is a chunk that sits in several places that
/// colour it differently, or that ends past where its text is coloured.
// shortcut: Rune tells neither the side nor the file of a chunk, so a chunk that is in both the old
// and the new text with different lines around it gets the per-chunk colours, not the whole text's.
pub fn changeset_highlighter(changeset: &Changeset) -> Highlighter {
    let texts: Vec<Text> = changeset
        .files
        .iter()
        .flat_map(|f| [&f.old, &f.new].into_iter().flatten().map(move |text| (f.path.as_str(), text)))
        .filter(|(_, text)| !text.is_empty())
        .map(|(path, text)| Text { lang: syntax::lang_for_path(path), text: as_chunked(text), whole: OnceCell::new() })
        .collect();
    // Highlighters from the same closure type share Rune's cache, so say what the output depends on.
    let mut key = DefaultHasher::new();
    for t in &texts {
        (t.lang, &t.text).hash(&mut key);
    }
    Highlighter::new(move |lang, chunk| highlight_chunk(&texts, lang, chunk)).key(key.finish())
}

/// The highlights of `chunk`: lines of one of `texts`, then maybe empty lines of padding.
fn highlight_chunk(texts: &[Text], lang: Option<&str>, chunk: &str) -> Vec<Highlight> {
    let lines = chunk.trim_end_matches('\n');
    if lang.is_none() || lines.is_empty() {
        return syntax::builtin(lang, chunk);
    }
    // Every line of a chunk ends in `\n`, so one of the `\n`s after `lines` ends the last of them.
    let blank = (chunk.len() - lines.len()).saturating_sub(1);
    let mut agreed: Option<Vec<Highlight>> = None;
    for text in texts.iter().filter(|t| t.lang == lang) {
        for found in text.find_all(lines, blank) {
            let whole = text.whole();
            if found.end > whole.coloured {
                return syntax::builtin(lang, chunk);
            }
            let first = whole.highlights.partition_point(|(r, _)| r.end <= found.start);
            let here: Vec<Highlight> = whole.highlights[first..]
                .iter()
                .take_while(|(r, _)| r.start < found.end)
                .map(|(r, paint)| (r.start.max(found.start) - found.start..r.end.min(found.end) - found.start, *paint))
                .collect();
            match &agreed {
                Some(before) if *before != here => return syntax::builtin(lang, chunk),
                Some(_) => {}
                None => agreed = Some(here),
            }
        }
    }
    agreed.unwrap_or_else(|| syntax::builtin(lang, chunk))
}
