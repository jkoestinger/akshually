//! Syntax colours of a Changeset's diff, taken from the whole text of each side of a file.
//!
//! Rune's diff view highlights a file 128 lines at a time, each chunk as if it were a whole
//! source, so a string or comment that crosses a chunk boundary loses its context (#106). The
//! Highlighter here knows the texts of the Changeset shown, finds the one a chunk is part of,
//! and answers with that part of the whole text's highlights.

use std::cell::OnceCell;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::ops::Range;

use rune_kit::syntax::{self, Highlight, Highlighter};

use akshually_core::Changeset;

/// How much of a text Rune's built-in highlighter colours (its `MAX_SOURCE`): past it a whole
/// text's highlights stop, so a chunk out there is highlighted on its own instead of drawn plain.
const COLOURED_BYTES: usize = 128 * 1024;

/// One side of a changed file.
struct Text {
    /// The highlighter language of the file's path.
    lang: Option<&'static str>,
    text: String,
    /// The highlights of the whole `text`, made when the first chunk of it is asked for.
    whole: OnceCell<Vec<Highlight>>,
}

impl Text {
    fn whole(&self) -> &[Highlight] {
        self.whole.get_or_init(|| syntax::builtin(self.lang, &self.text))
    }

    /// Where `lines` (whole lines, none empty at either end) are in this text, as a byte range.
    fn find(&self, lines: &str) -> Option<Range<usize>> {
        let bytes = self.text.as_bytes();
        self.text
            .match_indices(lines)
            .map(|(at, _)| at..at + lines.len())
            .find(|r| (r.start == 0 || bytes[r.start - 1] == b'\n') && (r.end == bytes.len() || bytes[r.end] == b'\n'))
    }
}

/// The Highlighter for the diff of `changeset`: every line gets the colours it has when the whole
/// text of its side is highlighted in one go, wherever it sits in the file.
///
/// A chunk that is in no text of `changeset`, or in none of its language, is highlighted on its
/// own, as Rune's built-in highlighter would.
pub fn changeset_highlighter(changeset: &Changeset) -> Highlighter {
    let texts: Vec<Text> = changeset
        .files
        .iter()
        .flat_map(|f| [&f.old, &f.new].into_iter().flatten().map(move |text| (f.path.as_str(), text)))
        .filter(|(_, text)| !text.is_empty())
        .map(|(path, text)| Text { lang: syntax::lang_for_path(path), text: text.clone(), whole: OnceCell::new() })
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
    for text in texts.iter().filter(|t| t.lang == lang) {
        let Some(found) = text.find(lines) else { continue };
        if text.text.len() > COLOURED_BYTES && found.end > COLOURED_BYTES {
            break;
        }
        let whole = text.whole();
        let first = whole.partition_point(|(r, _)| r.end <= found.start);
        return whole[first..]
            .iter()
            .take_while(|(r, _)| r.start < found.end)
            .map(|(r, paint)| (r.start.max(found.start) - found.start..r.end.min(found.end) - found.start, *paint))
            .collect();
    }
    syntax::builtin(lang, chunk)
}
