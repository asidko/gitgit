//! Domain types for the diff/preview viewer.
//!
//! Backend-agnostic and free of any highlighter or UI dependency: the backend
//! builds these (the fixtures pre-tokenize via [`crate::highlight`]; the real
//! backend tokenizes via [`crate::tokenize`]), and the `ui` layer renders them
//! read-only. A `Token` carries a
//! [`TokenKind`], not a baked color, so the theme owns the palette and the UI
//! stays pure.

/// Semantic class of a source token, mapped from the highlighter's scope stack.
/// The UI turns this into a concrete color via [`crate::theme`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    Keyword,
    Func,
    Str,
    Number,
    Type,
    Ident,
    Punct,
    Comment,
}

/// One styled run of source text within a single line.
#[derive(Clone, Debug)]
pub struct Token {
    pub text: String,
    pub kind: TokenKind,
}

/// Whether a diff line is unchanged context, an insertion, or a deletion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

/// One line of a unified-model diff. Side-by-side vs unified is a pure render
/// transform over a `Vec<DiffLine>`, so there is exactly one representation.
#[derive(Clone, Debug)]
pub struct DiffLine {
    pub old_no: Option<usize>,
    pub new_no: Option<usize>,
    pub kind: LineKind,
    pub tokens: Vec<Token>,
    /// `[start, end)` character offsets of an inline-changed span, for a stronger
    /// in-line highlight on a modified line. `None` when nothing is inline-marked.
    pub inline_hl: Option<(usize, usize)>,
    /// Zero-based index of the diff HUNK this line belongs to (the same ordering
    /// libgit2 walks). Drives per-hunk revert: the focused changed line's `hunk`
    /// selects which hunk the working-tree revert applies. Context/fixture lines
    /// default to 0.
    pub hunk: usize,
    /// `Some(n)` marks this as a SYNTHETIC fold marker standing in for `n` unchanged
    /// lines git omitted between hunks (inserted unconditionally by the read-only diff
    /// builder - there is no user toggle). Such a row carries no real source: it renders
    /// as a dim zigzag and, being a `Context` row with no line number, is skipped by
    /// selection/copy and is never a per-hunk revert target. `None` for every real line.
    pub fold: Option<usize>,
}

impl DiffLine {
    /// A synthetic fold marker standing in for `hidden` collapsed context lines.
    /// Both line numbers are `None` and `tokens` is empty - the renderer special-cases
    /// `fold.is_some()` and never reads them.
    pub fn fold_marker(hidden: usize) -> Self {
        DiffLine {
            old_no: None,
            new_no: None,
            kind: LineKind::Context,
            tokens: Vec::new(),
            inline_hl: None,
            hunk: 0,
            fold: Some(hidden),
        }
    }
}

/// A two-revision diff of a single file.
#[derive(Clone, Debug)]
pub struct FileDiff {
    pub path: String,
    pub old_rev: String,
    pub new_rev: String,
    pub lines: Vec<DiffLine>,
}

/// An unchanged file shown as a single-pane, syntax-highlighted source preview.
#[derive(Clone, Debug)]
pub struct SourceFile {
    pub path: String,
    pub lang: String,
    /// One tokenized line per source line.
    pub lines: Vec<Vec<Token>>,
}

/// A binary file the viewer cannot show as text: it carries the path and a
/// human-readable note (e.g. "Binary file differs"). A binary delta yields zero
/// patch body lines, so without this the viewer would draw a blank diff body
/// indistinguishable from a no-op change; this variant lets the renderer surface
/// an explicit notice instead.
#[derive(Clone, Debug)]
pub struct BinaryFile {
    pub path: String,
    pub note: String,
}

/// One blamed source line: the line's syntax tokens plus the commit that last
/// touched it. `commit` is the short hash (blank for an uncommitted working-tree
/// line) and `date` is pre-formatted, but `author` is the FULL name (the renderer
/// abbreviates it for the gutter, matching the `Commit` full-in-model convention);
/// an uncommitted line carries the "Not Committed Yet" author git reports.
#[derive(Clone, Debug)]
pub struct BlameLine {
    pub commit: String,
    pub author: String,
    pub date: String,
    pub tokens: Vec<Token>,
}

/// A file annotated with per-line git blame (Annotate with Git Blame): each line
/// pairs its source with the commit/author/date that last changed it. Read-only;
/// shown as an inspect overlay over the diff pane, like a revision Source.
#[derive(Clone, Debug)]
pub struct BlameFile {
    pub path: String,
    pub lines: Vec<BlameLine>,
}

/// What the top viewer is previewing for the selected file: a changed file's
/// diff, an unchanged file's source, a binary file's notice, or a blame overlay.
#[derive(Clone, Debug)]
pub enum FileView {
    Diff(FileDiff),
    Source(SourceFile),
    Binary(BinaryFile),
    Blame(BlameFile),
}

impl FileView {
    /// Number of body lines this preview occupies, for diff-scroll clamping.
    /// Pure: a diff counts its lines, a source its rows, a binary its single
    /// notice row. The single source for the scroll clamp's upper bound (`apply`
    /// reads it, never recomputes it).
    pub fn line_count(&self) -> usize {
        match self {
            FileView::Diff(d) => d.lines.len(),
            FileView::Source(s) => s.lines.len(),
            FileView::Binary(_) => 1,
            FileView::Blame(b) => b.lines.len(),
        }
    }

    /// Number of VISUAL rows the body renders in `side_by_side` mode. In unified
    /// (and for source/binary) this equals [`Self::line_count`]; in side-by-side a
    /// Removed line immediately followed by an Added line collapses to ONE row (a
    /// modified pair), so the visual count is smaller. The scroll clamp uses THIS as
    /// its upper bound so the user cannot scroll the body off into blank rows. The
    /// single source of the side-by-side pairing count, mirrored by `diff_view`'s
    /// `pair_rows`.
    pub fn visual_rows(&self, side_by_side: bool) -> usize {
        match self {
            FileView::Diff(d) if side_by_side => paired_row_count(&d.lines),
            _ => self.line_count(),
        }
    }
}

/// Count the side-by-side visual rows of `lines`: a Context line is one row; a changed
/// block (a maximal Removed run followed by a maximal Added run) is ZIPPED into
/// `max(rm_len, add_len)` rows (removed[k] opposite added[k], the longer run's tail
/// against a filler). MUST mirror `diff_view::pair_rows` exactly - it feeds the
/// side-by-side scroll clamp, so an overcount lets the body scroll past its last real
/// row into blank filler.
fn paired_row_count(lines: &[DiffLine]) -> usize {
    let mut rows = 0;
    let mut i = 0;
    while i < lines.len() {
        match lines[i].kind {
            LineKind::Context => {
                rows += 1;
                i += 1;
            }
            LineKind::Removed | LineKind::Added => {
                let rm_start = i;
                while i < lines.len() && lines[i].kind == LineKind::Removed {
                    i += 1;
                }
                let (rm_len, add_start) = (i - rm_start, i);
                while i < lines.len() && lines[i].kind == LineKind::Added {
                    i += 1;
                }
                rows += rm_len.max(i - add_start);
            }
        }
    }
    rows
}

