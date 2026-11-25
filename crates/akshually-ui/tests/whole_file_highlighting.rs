//! UI: syntax colours of a Changeset's diff come from the whole text of each side, not from the
//! 128-line chunks Rune hands its `Highlighter` (#106). The `Highlighter` akshually-ui gives the diff
//! is called as Rune calls it, with a chunk's text, and its paints are checked without drawing.

use rune_kit::syntax::{self, Highlight, Paint, Syntax};

use akshually_core::{Changeset, FileChange};
use akshually_ui::changeset_highlighter;

/// How many lines Rune highlights at a time.
const CHUNK: usize = 128;

fn changeset(files: Vec<FileChange>) -> Changeset {
    Changeset { base: "base".into(), files }
}

/// The text Rune hands the `Highlighter` for the chunk starting at line `first` (1-based): its
/// lines, each ended by a newline, and empty lines up to `CHUNK`.
fn chunk_of(text: &str, first: usize) -> String {
    let mut lines: Vec<&str> = text.lines().skip(first - 1).take(CHUNK).collect();
    lines.resize(CHUNK, "");
    lines.iter().map(|l| format!("{l}\n")).collect()
}

/// The paints of the chunk's line `n` (1-based within the chunk), each with the text it covers.
fn paints_of_line<'a>(chunk: &'a str, hl: &[Highlight], n: usize) -> Vec<(&'a str, Paint)> {
    let start: usize = chunk.split_inclusive('\n').take(n - 1).map(str::len).sum();
    let end = start + chunk.split_inclusive('\n').nth(n - 1).unwrap().trim_end_matches('\n').len();
    hl.iter()
        .filter(|(r, _)| r.start < end && r.end > start)
        .map(|(r, p)| (&chunk[r.start.max(start)..r.end.min(end)], *p))
        .collect()
}

fn is_string(p: &Paint) -> bool {
    *p == Paint::Kind(Syntax::String)
}

/// A Python file of `total` lines whose docstring opens on line 124 and closes on line 137.
fn python_with_docstring_across_line_128(total: usize) -> String {
    let lines: Vec<String> = (1..=total)
        .map(|n| match n {
            124 => "    \"\"\"Opens before the chunk boundary.".to_owned(),
            125..=136 => "    Args: while for False is prose in the docstring".to_owned(),
            137 => "    closes here.\"\"\"".to_owned(),
            138 => "while True:".to_owned(),
            _ => format!("x{n} = {n}"),
        })
        .collect();
    lines.join("\n") + "\n"
}

/// Lines 129-136 of the file are inside the docstring, and line 138 is code.
fn assert_docstring_is_string_and_code_is_code(chunk: &str, hl: &[Highlight]) {
    for n in 1..=8 {
        let paints = paints_of_line(chunk, hl, n);
        assert!(!paints.is_empty(), "docstring line {} is plain", 128 + n);
        assert!(
            paints.iter().all(|(_, p)| is_string(p)),
            "docstring line {} has non-string paints: {paints:?}",
            128 + n
        );
    }
    let code = paints_of_line(chunk, hl, 10);
    assert!(
        code.iter().any(|(t, p)| t.trim() == "while" && *p == Paint::Kind(Syntax::Keyword)),
        "code after the docstring is not code: {code:?}"
    );
}

#[test]
fn a_python_docstring_crossing_a_chunk_boundary_stays_a_string_in_the_next_chunk() {
    let text = python_with_docstring_across_line_128(300);
    let highlighter = changeset_highlighter(&changeset(vec![FileChange::text("a.py", None, Some(&text))]));
    let chunk = chunk_of(&text, 129);

    let hl = highlighter.highlight(Some("python"), &chunk);

    assert_docstring_is_string_and_code_is_code(&chunk, &hl);
}

#[test]
fn a_rust_block_comment_crossing_a_chunk_boundary_stays_a_comment_in_the_next_chunk() {
    let mut lines: Vec<String> = (1..=300).map(|n| format!("let x{n} = {n};")).collect();
    lines[125] = "/* a comment that opens on line 126".into();
    lines[136] = "closes here */".into();
    let text = lines.join("\n") + "\n";
    let highlighter = changeset_highlighter(&changeset(vec![FileChange::text("a.rs", None, Some(&text))]));
    let chunk = chunk_of(&text, 129);

    let hl = highlighter.highlight(Some("rust"), &chunk);

    for n in 1..=9 {
        let paints = paints_of_line(&chunk, &hl, n);
        let comment = Paint::Kind(Syntax::Comment);
        assert!(!paints.is_empty() && paints.iter().all(|(_, p)| *p == comment), "line {}: {paints:?}", 128 + n);
    }
    let code = paints_of_line(&chunk, &hl, 10);
    assert!(code.iter().any(|(t, _)| t.trim() == "let"), "code after the comment is plain: {code:?}");
}

#[test]
fn the_old_side_of_a_changed_file_is_highlighted_from_its_whole_old_text() {
    let old = python_with_docstring_across_line_128(300);
    let new = old.replace("x1 = 1\n", "x1 = 2\n");
    let file = FileChange::text("a.py", Some(&old), Some(&new));
    let highlighter = changeset_highlighter(&changeset(vec![file]));
    let chunk = chunk_of(&old, 129);

    let hl = highlighter.highlight(Some("python"), &chunk);

    assert_docstring_is_string_and_code_is_code(&chunk, &hl);
}

#[test]
fn the_padded_last_chunk_of_a_file_is_highlighted_from_the_whole_text() {
    // 140 lines: the chunk of lines 129-256 has 12 real lines and 116 of padding.
    let text = python_with_docstring_across_line_128(140);
    let highlighter = changeset_highlighter(&changeset(vec![FileChange::text("a.py", None, Some(&text))]));
    let chunk = chunk_of(&text, 129);

    let hl = highlighter.highlight(Some("python"), &chunk);

    assert_docstring_is_string_and_code_is_code(&chunk, &hl);
}

#[test]
fn a_chunk_found_in_no_text_is_highlighted_on_its_own() {
    let text = python_with_docstring_across_line_128(300);
    let highlighter = changeset_highlighter(&changeset(vec![FileChange::text("a.py", None, Some(&text))]));
    let other = "def f():\n    return 'x'\n".repeat(64);

    let hl = highlighter.highlight(Some("python"), &other);

    assert!(!hl.is_empty());
    assert_eq!(*hl, *syntax::builtin(Some("python"), &other));
}

#[test]
fn a_language_the_built_in_highlighter_does_not_know_stays_plain() {
    let text = "plain words\n".repeat(200);
    let highlighter = changeset_highlighter(&changeset(vec![FileChange::text("a.unknown", None, Some(&text))]));

    let hl = highlighter.highlight(None, &chunk_of(&text, 1));

    assert!(hl.is_empty());
}

#[test]
fn highlights_follow_the_changeset_they_were_built_from() {
    // The same chunk text in both Changesets; only the lines before it differ.
    let opened = python_with_docstring_across_line_128(300);
    let not_opened = opened.replace("    \"\"\"Opens before", "    # Opens before");
    let chunk = chunk_of(&opened, 129);
    let before = changeset_highlighter(&changeset(vec![FileChange::text("a.py", None, Some(&opened))]));
    let first = before.highlight(Some("python"), &chunk);

    let after = changeset_highlighter(&changeset(vec![FileChange::text("a.py", None, Some(&not_opened))]));
    let second = after.highlight(Some("python"), &chunk);

    assert!(first.iter().any(|(r, p)| is_string(p) && r.start == 0), "the docstring continues in the first");
    assert_ne!(*first, *second, "the second answered from the first Changeset's cache");
}

#[test]
fn returned_ranges_are_relative_to_the_chunk_and_within_it() {
    let text = python_with_docstring_across_line_128(140);
    let highlighter = changeset_highlighter(&changeset(vec![FileChange::text("a.py", None, Some(&text))]));
    let chunk = chunk_of(&text, 129);

    let hl = highlighter.highlight(Some("python"), &chunk);

    assert!(hl.iter().all(|(r, _)| r.end <= chunk.len()), "ranges past the chunk: {hl:?}");
    assert_eq!(hl[0].0.start, 0, "the docstring's colour starts at the chunk's first byte");
}

#[test]
fn a_chunk_past_where_rune_stops_colouring_a_dense_text_is_highlighted_on_its_own() {
    // Under the byte limit, yet over Rune's cap on coloured ranges: the whole text's highlights stop early.
    let lines: Vec<String> = (1..=14000).map(|n| format!("a{n}=1")).collect();
    let text = lines.join("\n") + "\n";
    assert!(text.len() < 128 * 1024 && syntax::builtin(Some("python"), &text).len() == 32 * 1024, "not dense enough");
    let highlighter = changeset_highlighter(&changeset(vec![FileChange::text("a.py", None, Some(&text))]));
    let chunk = chunk_of(&text, 13441);

    let hl = highlighter.highlight(Some("python"), &chunk);

    assert!(!hl.is_empty());
    assert_eq!(*hl, *syntax::builtin(Some("python"), &chunk));
}

#[test]
fn a_chunk_past_the_byte_limit_is_highlighted_on_its_own() {
    let lines: Vec<String> = (1..=8000).map(|n| format!("value_{n} = {n}")).collect();
    let text = lines.join("\n") + "\n";
    assert!(text.len() > 128 * 1024, "not long enough");
    let highlighter = changeset_highlighter(&changeset(vec![FileChange::text("a.py", None, Some(&text))]));
    let chunk = chunk_of(&text, 7937);

    let hl = highlighter.highlight(Some("python"), &chunk);

    assert!(!hl.is_empty());
    assert_eq!(*hl, *syntax::builtin(Some("python"), &chunk));
}

#[test]
fn a_chunk_of_a_file_with_crlf_line_endings_is_highlighted_from_the_whole_text() {
    let text = python_with_docstring_across_line_128(300).replace('\n', "\r\n");
    let highlighter = changeset_highlighter(&changeset(vec![FileChange::text("a.py", None, Some(&text))]));
    let chunk = chunk_of(&text, 129);
    assert!(!chunk.contains('\r'), "Rune strips the carriage returns before it chunks");

    let hl = highlighter.highlight(Some("python"), &chunk);

    assert_docstring_is_string_and_code_is_code(&chunk, &hl);
}

/// A Rust file of 130 lines of code, with `edit` applied to its lines.
fn rust_130_lines(edit: impl FnOnce(&mut Vec<String>)) -> String {
    let mut lines: Vec<String> = (1..=130).map(|n| format!("let x{n} = {n};")).collect();
    edit(&mut lines);
    lines.join("\n") + "\n"
}

#[test]
fn the_padded_last_chunk_is_taken_from_the_end_of_the_text_not_from_an_earlier_copy_of_its_lines() {
    // Lines 129-130 are code on lines 5-6, but they end the file inside a comment opened on line 128.
    let text = rust_130_lines(|lines| {
        lines[4] = "let a = 1;".into();
        lines[5] = "let b = 2;".into();
        lines[127] = "/* a comment to the end of the file".into();
        lines[128] = "let a = 1;".into();
        lines[129] = "let b = 2;".into();
    });
    let highlighter = changeset_highlighter(&changeset(vec![FileChange::text("a.rs", None, Some(&text))]));
    let chunk = chunk_of(&text, 129);

    let hl = highlighter.highlight(Some("rust"), &chunk);

    let comment = Paint::Kind(Syntax::Comment);
    for n in 1..=2 {
        let paints = paints_of_line(&chunk, &hl, n);
        assert!(!paints.is_empty() && paints.iter().all(|(_, p)| *p == comment), "line {}: {paints:?}", 128 + n);
    }
}

#[test]
fn the_padded_last_chunk_that_also_sits_inside_a_comment_earlier_is_not_a_comment() {
    // Lines 129-130 are code, but they also appear on lines 6-7, inside a block comment.
    let text = rust_130_lines(|lines| {
        lines[4] = "/* a comment".into();
        lines[5] = "let a = 1;".into();
        lines[6] = "let b = 2;".into();
        lines[7] = "closes */".into();
        lines[128] = "let a = 1;".into();
        lines[129] = "let b = 2;".into();
    });
    let highlighter = changeset_highlighter(&changeset(vec![FileChange::text("a.rs", None, Some(&text))]));
    let chunk = chunk_of(&text, 129);

    let hl = highlighter.highlight(Some("rust"), &chunk);

    let paints = paints_of_line(&chunk, &hl, 1);
    assert!(paints.iter().any(|(t, p)| t.trim() == "let" && *p == Paint::Kind(Syntax::Keyword)), "{paints:?}");
}

#[test]
fn a_chunk_that_is_a_comment_in_one_place_and_code_in_another_is_highlighted_on_its_own() {
    // The same 128 whole lines twice: first inside a block comment, then as code.
    let block: String = (1..=128).map(|n| format!("let x{n} = {n};\n")).collect();
    let text = format!("/*\n{block}*/\n{block}");
    let highlighter = changeset_highlighter(&changeset(vec![FileChange::text("a.rs", None, Some(&text))]));

    let hl = highlighter.highlight(Some("rust"), &block);

    assert!(!hl.is_empty());
    assert_eq!(*hl, *syntax::builtin(Some("rust"), &block));
}

#[test]
fn a_chunk_ending_in_a_blank_line_of_the_file_is_still_highlighted_from_the_whole_text() {
    // Line 256 is blank, and so are the last two lines of the file.
    let mut lines: Vec<String> = python_with_docstring_across_line_128(300).lines().map(str::to_owned).collect();
    lines[255] = String::new();
    lines.extend([String::new(), String::new()]);
    let text = lines.join("\n") + "\n";
    let highlighter = changeset_highlighter(&changeset(vec![FileChange::text("a.py", None, Some(&text))]));
    let chunk = chunk_of(&text, 129);

    let hl = highlighter.highlight(Some("python"), &chunk);

    assert_docstring_is_string_and_code_is_code(&chunk, &hl);
}

#[test]
fn a_padded_last_chunk_of_a_file_ending_in_blank_lines_is_still_highlighted_from_the_whole_text() {
    // Lines 139-140 are blank: real blank lines, then padding.
    let mut lines: Vec<String> = python_with_docstring_across_line_128(138).lines().map(str::to_owned).collect();
    lines.extend([String::new(), String::new()]);
    let text = lines.join("\n") + "\n";
    let highlighter = changeset_highlighter(&changeset(vec![FileChange::text("a.py", None, Some(&text))]));
    let chunk = chunk_of(&text, 129);

    let hl = highlighter.highlight(Some("python"), &chunk);

    assert_docstring_is_string_and_code_is_code(&chunk, &hl);
}
