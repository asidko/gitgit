//! The ONE syntax-highlight code path for a [`FileView`].
//!
//! A real git backend returns raw, un-highlighted diff/source lines; this pure
//! transform runs every line through [`crate::highlight`] so the UI always sees
//! tokenized text. Fixtures pre-tokenize at construction time, so for them the
//! transform is a NO-OP (their tokens already carry kinds). This keeps a single
//! highlight entry point: the fixture path and the future real path converge here.
//!
//! PURE: depends only on `diff`/`highlight`. No backend/ui/store/Msg.

use crate::diff::{BlameFile, DiffLine, FileDiff, FileView, SourceFile};
use crate::highlight::{highlight_block, highlight_line, lang_of};

/// Syntax-highlight a freshly-loaded [`FileView`] in place. A `Source` view is
/// re-tokenized as a whole block (state carries across lines); a `Diff` view
/// re-tokenizes each line by its language. Fixtures are already tokenized, so the
/// real backend is the caller that needs this; calling it on tokenized input is
/// idempotent in shape (it re-derives the same tokens from the rendered text).
pub fn highlight_view(view: FileView, lang: &str) -> FileView {
    match view {
        FileView::Source(s) => FileView::Source(highlight_source(s, lang)),
        FileView::Diff(d) => FileView::Diff(highlight_diff(d, lang)),
        FileView::Blame(b) => FileView::Blame(highlight_blame(b, lang)),
        // A binary view carries no text to tokenize; pass it through untouched.
        FileView::Binary(b) => FileView::Binary(b),
    }
}

/// Highlight a [`FileView`] deriving its language from the view itself: a `Source`
/// carries its `lang`, a `Diff` its `path` (extension). The ONE call the loader
/// makes so the language lookup lives next to the highlight pass, not at the
/// call site.
pub fn highlight(view: FileView) -> FileView {
    let lang = match &view {
        FileView::Source(s) => s.lang.clone(),
        FileView::Diff(d) => lang_of(&d.path),
        FileView::Blame(b) => lang_of(&b.path),
        // No language: a binary view has nothing to highlight.
        FileView::Binary(_) => String::new(),
    };
    highlight_view(view, &lang)
}

/// Re-tokenize a source preview as one `lang` block.
fn highlight_source(mut s: SourceFile, lang: &str) -> SourceFile {
    let src: String = s
        .lines
        .iter()
        .map(|tokens| line_text(tokens))
        .collect::<Vec<_>>()
        .join("\n");
    s.lines = highlight_block(lang, &src);
    s
}

/// Re-tokenize every diff line by `lang`, preserving line numbers/kind/inline.
fn highlight_diff(mut d: FileDiff, lang: &str) -> FileDiff {
    for line in &mut d.lines {
        line.tokens = highlight_line(lang, &diff_line_text(line));
    }
    d
}

/// Re-tokenize every blame line by `lang`, preserving its commit/author/date gutter.
/// Highlighted as a whole block (multi-line string/comment state carries across rows),
/// then zipped back onto each line; a count mismatch keeps the shorter run.
fn highlight_blame(mut b: BlameFile, lang: &str) -> BlameFile {
    let src: String = b
        .lines
        .iter()
        .map(|l| line_text(&l.tokens))
        .collect::<Vec<_>>()
        .join("\n");
    for (line, tokens) in b.lines.iter_mut().zip(highlight_block(lang, &src)) {
        line.tokens = tokens;
    }
    b
}

/// Flatten a tokenized source row back to its raw text.
fn line_text(tokens: &[crate::diff::Token]) -> String {
    tokens.iter().map(|t| t.text.as_str()).collect()
}

/// Flatten a tokenized diff line back to its raw text.
fn diff_line_text(line: &DiffLine) -> String {
    line.tokens.iter().map(|t| t.text.as_str()).collect()
}
