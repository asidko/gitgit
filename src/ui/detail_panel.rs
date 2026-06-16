//! Bottom-right commit detail: subject, author/committer signatures, and the
//! "In N branches" containment block.
//!
//! Collapsed (default): the header prefix then the comma-joined branch names. If
//! the names fit on the line they render in full with NO link; only when they
//! would overflow are they truncated with "..." and a "Show all" link offered.
//! Expanded: a "Hide" link, then one branch per line. The pane scrolls
//! vertically (`view.detail_scroll`) when the content outgrows its height.
//!
//! PURE: imports only `model`/`view_state`/`theme`. The collapsed line and the
//! link's hit-test rect are decided by ONE helper ([`collapsed_layout`]) that both
//! the renderer and [`branches_link_rect`] (which [`super::layout`] reuses) call,
//! so render and click never disagree about whether the link exists or where.

use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::widgets::{centered_notice, pad};
use crate::model::{
    detail_has_committer_line, status_color, CommitDetail, FileStatus, Signature, Status,
    WorkingSummary,
};
use crate::theme::Theme;
use crate::view_state::{DetailSel, ViewState};


/// Left indent (in cells) shared by every detail line.
const INDENT: &str = "  ";
/// Collapsed-list link label.
const SHOW_ALL: &str = "Show all";
/// Expanded-list link label.
const HIDE: &str = "Hide";
/// Truncation marker for the collapsed branch line (plain ASCII).
const ELLIPSIS: &str = "...";

/// The collapsed branch line's two shapes, decided once from the pane width.
/// `Fits` -> the full names render with no link; `Truncated` -> the cut names
/// render followed by a "Show all" link. The single decision both the renderer
/// and [`branches_link_rect`] consume so they never drift.
enum Collapsed {
    /// All names fit: render them in full, emit no link.
    Fits(String),
    /// Names overflow: render the truncated form, then a "Show all" link.
    Truncated(String),
}

pub fn render(
    frame: &mut Frame,
    area: Rect,
    detail: Option<&CommitDetail>,
    view: &ViewState,
    status: &Status,
) {
    // No detail yet. B-3: a selected commit always carries a cheap synchronous
    // placeholder detail, so `None` here means the repo is still empty - during
    // startup show a centered notice rather than flashing an empty pane; an error
    // or a settled-empty repo leaves the painted background.
    let Some(d) = detail else {
        if *status == Status::Loading {
            centered_notice(frame, area, "Loading commit...", Theme::TEXT_DIM, Theme::BG);
        }
        return;
    };
    let mut lines = wrapped_lines(d, view, area);
    if let Some(sel) = view.detail_sel {
        paint_selection(&mut lines, &sel);
    }
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((view.detail_scroll as u16, 0))
            .style(Style::default().bg(Theme::BG)),
        area,
    );
}

// -- wrapping + read-only text selection -------------------------------------
//
// The detail content is wrapped to the pane width so long lines (a long subject,
// a long signature, a long branch name) fold instead of clipping. Selection +
// click mapping operate on these WRAPPED visual lines, so the runtime and the
// renderer agree cell-for-cell. `wrapped_lines` is the single layout both share.

/// The detail content wrapped to `area.width`: every logical line that overflows
/// folds into one-or-more visual lines (word-aware, falling back to a hard break for
/// a single over-long token). A line that already fits is returned unchanged, so the
/// common case stays byte-identical to the pre-wrap render.
pub fn wrapped_lines(d: &CommitDetail, view: &ViewState, area: Rect) -> Vec<Line<'static>> {
    wrapped_with_skip(d, view, area)
        .into_iter()
        .map(|(line, _)| line)
        .collect()
}

/// Wrapped visual rows paired with the count of leading SYNTHETIC hanging-indent cells
/// to skip when copying the row's text (0 for a logical line's first row, whose indent is
/// real source; `indent` for a continuation row, whose indent the wrap re-injects only for
/// visual alignment). [`selected_text`] strips this so the clipboard gets source text, not
/// the rendered padding. The single wrap walk both copy and render share.
fn wrapped_with_skip(d: &CommitDetail, view: &ViewState, area: Rect) -> Vec<(Line<'static>, usize)> {
    let width = area.width as usize;
    detail_lines(d, view, area)
        .iter()
        .flat_map(|line| wrap_line(line, width))
        .collect()
}

/// The plain text of a rendered line (its spans concatenated) - the selectable text.
fn line_text(line: &Line) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Char count of wrapped visual `row` (the column bound for a click / line-select).
pub fn line_len(d: &CommitDetail, view: &ViewState, area: Rect, row: usize) -> usize {
    wrapped_lines(d, view, area)
        .get(row)
        .map_or(0, |l| line_text(l).chars().count())
}

/// Map a terminal click at `(col, row)` to a wrapped `(visual_row, char_col)`, clamped
/// into the rendered content (so a click in the blank tail of a line lands at its end,
/// and a click below the content lands on the last line). `None` only when empty.
pub fn click_pos(
    d: &CommitDetail,
    view: &ViewState,
    area: Rect,
    col: u16,
    row: u16,
) -> Option<(usize, usize)> {
    let lines = wrapped_lines(d, view, area);
    if lines.is_empty() {
        return None;
    }
    let r = ((row.saturating_sub(area.y)) as usize + view.detail_scroll).min(lines.len() - 1);
    let maxc = line_text(&lines[r]).chars().count();
    let c = ((col.saturating_sub(area.x)) as usize).min(maxc);
    Some((r, c))
}

/// The word span `[start, end)` at wrapped `(row, col)` (double-click): the maximal run
/// of word chars (alphanumeric or `_`) around the click, or the single char when it is
/// not a word char. Returns `(row, start, end)`; `None` when the row does not exist.
pub fn word_span(
    d: &CommitDetail,
    view: &ViewState,
    area: Rect,
    row: usize,
    col: usize,
) -> Option<(usize, usize, usize)> {
    let lines = wrapped_lines(d, view, area);
    let chars: Vec<char> = line_text(lines.get(row)?).chars().collect();
    if chars.is_empty() {
        return Some((row, 0, 0));
    }
    let c = col.min(chars.len() - 1);
    let is_word = |ch: char| ch.is_alphanumeric() || ch == '_';
    if is_word(chars[c]) {
        let mut s = c;
        while s > 0 && is_word(chars[s - 1]) {
            s -= 1;
        }
        let mut e = c + 1;
        while e < chars.len() && is_word(chars[e]) {
            e += 1;
        }
        Some((row, s, e))
    } else {
        Some((row, c, c + 1))
    }
}

/// The text spanned by the current detail selection, joined by `\n` across wrapped
/// rows (for the system clipboard). `None` when there is no selection or it is empty.
pub fn selected_text(d: &CommitDetail, view: &ViewState, area: Rect) -> Option<String> {
    let sel = view.detail_sel?;
    if sel.is_empty() {
        return None;
    }
    let ((sr, sc), (er, ec)) = sel.span();
    let rows: Vec<(Vec<char>, usize)> = wrapped_with_skip(d, view, area)
        .iter()
        .map(|(l, skip)| (line_text(l).chars().collect(), *skip))
        .collect();
    let last = rows.len().saturating_sub(1);
    let mut out = String::new();
    for (r, (row, skip)) in rows.iter().enumerate().take(er.min(last) + 1).skip(sr) {
        let (from, to) = if sr == er {
            (sc, ec)
        } else if r == sr {
            (sc, row.len())
        } else if r == er {
            (0, ec)
        } else {
            (0, row.len())
        };
        // Never copy a continuation row's synthetic hanging indent (cols 0..skip): it is
        // render padding, not source text.
        let from = from.max(*skip);
        if r > sr {
            out.push('\n');
        }
        out.extend(row.iter().skip(from).take(to.min(row.len()).saturating_sub(from)));
    }
    Some(out)
}

/// Repaint the selection band: every cell inside `sel` (in wrapped coords) takes the
/// editor's brighter text-selection background ([`Theme::SELECTION_EDIT`], the same one
/// the diff editor uses) so a fresh text grab reads distinctly from a passive row band;
/// the glyph + foreground colour are untouched so the text stays readable and syntax /
/// link colours survive. Only rows the selection touches rebuild.
fn paint_selection(lines: &mut [Line<'static>], sel: &DetailSel) {
    let ((sr, _), (er, _)) = sel.span();
    for (r, line) in lines.iter_mut().enumerate() {
        if r < sr || r > er {
            continue;
        }
        let cells: Vec<(char, Style)> = line_cells(line)
            .into_iter()
            .enumerate()
            .map(|(c, (ch, st))| {
                if sel.contains(r, c) {
                    (ch, st.bg(Theme::SELECTION_EDIT))
                } else {
                    (ch, st)
                }
            })
            .collect();
        *line = cells_to_line(&cells);
    }
}

/// Flatten a line into per-char `(glyph, style)` cells (each char inherits its span's
/// style) so it can be wrapped or have its selection band painted column-by-column.
fn line_cells(line: &Line) -> Vec<(char, Style)> {
    line.spans
        .iter()
        .flat_map(|s| s.content.chars().map(move |c| (c, s.style)))
        .collect()
}

/// Re-group `(glyph, style)` cells into a [`Line`], merging consecutive same-style runs
/// back into spans (so the render stays compact).
fn cells_to_line(cells: &[(char, Style)]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut cur: Option<Style> = None;
    for &(ch, st) in cells {
        if cur != Some(st) {
            if let Some(prev) = cur {
                spans.push(Span::styled(std::mem::take(&mut buf), prev));
            }
            cur = Some(st);
        }
        buf.push(ch);
    }
    if let Some(st) = cur {
        spans.push(Span::styled(buf, st));
    }
    Line::from(spans)
}

/// Wrap one logical line to `width` columns. A line within the width is returned as-is
/// (byte-identical clone). Otherwise it folds with a HANGING INDENT: the line's own
/// leading whitespace is repeated on every continuation row so a wrapped subject /
/// signature stays aligned under the first row instead of falling back to column 0 (the
/// padding is respected across the wrap). The content folds at the last space before the
/// limit, or hard-breaks a single over-long token. A full-width fill band (the subject
/// header, whose cells carry a background) is re-padded to `width` on every row so the
/// band stays rectangular. Style is preserved per char.
fn wrap_line(line: &Line<'static>, width: usize) -> Vec<(Line<'static>, usize)> {
    let cells = line_cells(line);
    if width == 0 || cells.len() <= width {
        return vec![(line.clone(), 0)];
    }
    // The leading whitespace defines the hanging indent; clamp so a pathological indent
    // wider than the pane still leaves one content column.
    let indent = cells
        .iter()
        .take_while(|(c, _)| *c == ' ')
        .count()
        .min(width.saturating_sub(1));
    let prefix = &cells[..indent];
    let content = &cells[indent..];
    let avail = width - indent;
    // A backgrounded first cell marks the full-width header band: pad each wrapped row
    // back out to `width` so the band does not go ragged where it folds.
    let fill = cells.first().is_some_and(|(_, st)| st.bg.is_some());
    let pad_style = cells.first().map_or_else(Style::default, |(_, st)| *st);

    let mut rows: Vec<(Line<'static>, usize)> = Vec::new();
    let mut i = 0;
    while i < content.len() {
        let end = i + avail;
        let chunk_end = if end >= content.len() {
            content.len()
        } else if content[end].0 == ' ' {
            // The column boundary lands on a space: the window holds whole words.
            end
        } else {
            // Mid-word: back up to the last space that still leaves a non-blank chunk;
            // else hard-break the single over-long token at the boundary.
            (i..end)
                .rev()
                .find(|&p| content[p].0 == ' ' && content[i..p].iter().any(|c| c.0 != ' '))
                .unwrap_or(end)
        };
        let mut row: Vec<(char, Style)> = prefix.to_vec();
        row.extend_from_slice(&content[i..chunk_end]);
        if fill {
            row.resize(width, (' ', pad_style));
        }
        // The first row's leading indent is the source's own whitespace (real); every
        // continuation row's is the re-injected hanging indent (synthetic -> copy skips it).
        let skip = if rows.is_empty() { 0 } else { indent };
        rows.push((cells_to_line(&row), skip));
        // Advance past the chunk, skipping a single break space at the fold.
        i = if chunk_end < content.len() && content[chunk_end].0 == ' ' {
            chunk_end + 1
        } else {
            chunk_end
        };
    }
    rows
}

/// Hit-test rect of the "Show all"/"Hide" link, or `None` when there is no link:
/// the commit has no containing branches, OR the collapsed names fit in full, OR
/// the header row has been scrolled off the top of the pane. The SINGLE geometry
/// source: render and [`super::layout`] both call it (with the same `area` and
/// `view.detail_scroll`), so the clickable region always matches the drawn link.
/// PURE.
pub fn branches_link_rect(detail: &CommitDetail, view: &ViewState, area: Rect) -> Option<Rect> {
    if detail.containing_branches.is_empty() {
        return None;
    }
    let prefix = header_prefix(detail);
    // Collapsed renders `prefix + names + " " + link` only when truncated;
    // expanded renders `prefix + link`. A fitting collapsed line has no link.
    let names_offset = if view.branches_expanded {
        0
    } else {
        match collapsed_layout(detail, area) {
            Collapsed::Fits(_) => return None,
            Collapsed::Truncated(names) => names.chars().count() + 1,
        }
    };
    // The header's VISUAL row = the wrapped height of every line above it (render now
    // folds the subject / signature, so a logical row index would drift below the drawn
    // link). The header line itself fits the width - collapsed names are truncated to
    // fit, expanded is short - so it occupies exactly one visual row. Shift up by the
    // scroll offset; if it scrolls above the pane top there is nothing to click.
    let width = area.width as usize;
    let header_idx = branches_header_row(detail) as usize;
    let content_row: usize = detail_lines(detail, view, area)
        .iter()
        .take(header_idx)
        .map(|line| wrap_line(line, width).len())
        .sum();
    let visible_row = content_row.checked_sub(view.detail_scroll)?;
    let x = area
        .x
        .saturating_add(prefix.chars().count() as u16)
        .saturating_add(names_offset as u16);
    Some(Rect {
        x,
        y: area.y.saturating_add(visible_row as u16),
        width: link_label(view).chars().count() as u16,
        height: 1,
    })
}

/// Build the full ordered content lines of the detail panel. Single source for
/// both rendering and the scroll-clamp line count, so they cannot disagree.
fn detail_lines(d: &CommitDetail, view: &ViewState, area: Rect) -> Vec<Line<'static>> {
    let w = area.width as usize;
    let dim = Style::default().fg(Theme::TEXT_DIM);
    let text = Style::default().fg(Theme::TEXT);
    let link = Style::default().fg(Theme::LINK);

    let mut lines = vec![Line::raw("")];

    // The synthetic "<current>" row shows the uncommitted-changes summary instead of a
    // commit subject/hash/author (it is not an authored commit).
    if let Some(ws) = &d.working {
        working_lines(ws, w, &mut lines);
        return lines;
    }

    // Subject as a full-width faint header band.
    lines.push(Line::from(Span::styled(
        pad(&format!("{INDENT}{}", d.subject), w),
        Style::default().bg(Theme::BG_TOOLBAR).fg(Theme::TEXT).bold(),
    )));
    lines.push(Line::raw(""));

    // <hash> <author> <email> on <when>
    lines.push(sig_line(&d.short_hash, &d.author, dim, text, link));
    // "committed by ..." only when the committer differs from the author.
    if detail_has_committer_line(d) {
        lines.push(committer_line(&d.committer, dim, text, link));
    }
    lines.push(Line::raw(""));

    // "In N branches" containment block.
    branches_lines(d, view, area, dim, link, &mut lines);
    lines
}

/// The "<current>" row's detail body: a full-width header band, the current branch
/// (when on one), then the per-status working-tree counts colored like the files pane
/// / log badge (added green, changed blue, deleted red). Replaces the commit
/// subject/hash/author block, which the synthetic working row has none of.
fn working_lines(ws: &WorkingSummary, w: usize, lines: &mut Vec<Line<'static>>) {
    lines.push(Line::from(Span::styled(
        pad(&format!("{INDENT}Uncommitted changes"), w),
        Style::default().bg(Theme::BG_TOOLBAR).fg(Theme::TEXT).bold(),
    )));
    // Counts FIRST (the primary info) so they stay visible in a short detail pane; the
    // current branch follows. Labels left-padded to a fixed width so the numbers align.
    let count_line = |label: &str, n: usize, st: FileStatus| {
        // A zero count carries no status, so it stays dimmed; only a nonzero count takes the
        // status color (changed blue, added green, deleted red) like the files pane.
        let num = if n == 0 { Theme::TEXT_DIM } else { status_color(st) };
        Line::from(vec![
            Span::styled(format!("{INDENT}{label:<9}"), Style::default().fg(Theme::TEXT_DIM)),
            Span::styled(n.to_string(), Style::default().fg(num)),
        ])
    };
    lines.push(count_line("Changed:", ws.changed, FileStatus::Modified));
    lines.push(count_line("Added:", ws.added, FileStatus::Added));
    lines.push(count_line("Deleted:", ws.deleted, FileStatus::Deleted));
    if let Some(branch) = &ws.branch {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            format!("{INDENT}On branch {branch}"),
            Style::default().fg(Theme::TEXT_DIM),
        )));
    }
}

/// Append the "In N branches" block to `lines`. Collapsed: the header prefix then
/// either the full names (no link) or the truncated names + "Show all" link.
/// Expanded: the header prefix + "Hide" link, then one dimmed branch per line.
fn branches_lines(
    d: &CommitDetail,
    view: &ViewState,
    area: Rect,
    dim: Style,
    link: Style,
    lines: &mut Vec<Line<'static>>,
) {
    if d.containing_branches.is_empty() {
        return;
    }
    let prefix = header_prefix(d);

    if view.branches_expanded {
        lines.push(Line::from(vec![
            Span::styled(prefix, dim),
            Span::styled(HIDE.to_string(), link),
        ]));
        for name in &d.containing_branches {
            lines.push(Line::from(Span::styled(format!("{INDENT}{name}"), dim)));
        }
    } else {
        let mut spans = vec![Span::styled(prefix, dim)];
        match collapsed_layout(d, area) {
            Collapsed::Fits(names) => spans.push(Span::styled(names, dim)),
            Collapsed::Truncated(names) => {
                spans.push(Span::styled(names, dim));
                spans.push(Span::raw(" "));
                spans.push(Span::styled(SHOW_ALL.to_string(), link));
            }
        }
        lines.push(Line::from(spans));
    }
}

/// Decide the collapsed branch line for `detail` at the pane width: the names fit
/// in full (no link) when they sit within the width after the prefix; otherwise
/// they are truncated to leave room for the trailing " Show all" link. The single
/// fits/truncation rule shared by [`branches_lines`] and [`branches_link_rect`].
fn collapsed_layout(detail: &CommitDetail, area: Rect) -> Collapsed {
    let names = detail.containing_branches.join(", ");
    let width = area.width as usize;
    let prefix_w = header_prefix(detail).chars().count();
    let full = width.saturating_sub(prefix_w);
    if names.chars().count() <= full {
        return Collapsed::Fits(names);
    }
    // Reserve the link plus its separating space when we must truncate.
    let budget = full.saturating_sub(SHOW_ALL.chars().count() + 1);
    Collapsed::Truncated(truncate_ascii(&names, budget))
}

/// The detail row (relative to `area.y`) the "In N branches" header sits on:
/// blank, subject, blank, sig, [committer], blank -> header. Single source for
/// both the rendered line order and the link rect.
fn branches_header_row(detail: &CommitDetail) -> u16 {
    let committer = if detail_has_committer_line(detail) { 1 } else { 0 };
    // 0 blank, 1 subject, 2 blank, 3 sig, (committer), then blank, then header.
    4 + committer + 1
}

/// `"In 1 branch: "` / `"In N branches: "` - the leading text before the link or
/// names. Singular when exactly one branch contains the commit, plural otherwise.
fn header_prefix(detail: &CommitDetail) -> String {
    let n = detail.containing_branches.len();
    let noun = if n == 1 { "branch" } else { "branches" };
    format!("{INDENT}In {n} {noun}: ")
}

/// Link text for the current expand state.
fn link_label(view: &ViewState) -> &'static str {
    if view.branches_expanded {
        HIDE
    } else {
        SHOW_ALL
    }
}

fn sig_line(hash: &str, s: &Signature, dim: Style, text: Style, link: Style) -> Line<'static> {
    let mut spans = vec![
        Span::raw(INDENT),
        Span::styled(format!("{hash} "), dim),
        Span::styled(format!("{} ", s.name), text),
    ];
    spans.extend(email_span(s, link));
    // No date for the synthetic working row (when == ""): skip the orphan " on ".
    if !s.when.is_empty() {
        spans.push(Span::styled(format!(" on {}", s.when), dim));
    }
    Line::from(spans)
}

fn committer_line(s: &Signature, dim: Style, text: Style, link: Style) -> Line<'static> {
    let mut spans = vec![
        Span::raw(INDENT),
        Span::styled("committed by ", dim),
        Span::styled(format!("{} ", s.name), text),
    ];
    spans.extend(email_span(s, link));
    if !s.when.is_empty() {
        spans.push(Span::styled(format!(" on {}", s.when), dim));
    }
    Line::from(spans)
}

/// The `<email>` chip, or nothing when the signature carries no email (commits
/// whose detail is derived from the log row do not have one).
fn email_span(s: &Signature, link: Style) -> Option<Span<'static>> {
    (!s.email.is_empty()).then(|| Span::styled(format!("<{}> ", s.email), link))
}

/// Truncate `s` to `max` columns, appending an ASCII `...` when cut (the font has
/// no ellipsis glyph). `max <= 3` collapses to just the marker.
fn truncate_ascii(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    if max <= ELLIPSIS.len() {
        return ELLIPSIS.to_string();
    }
    let kept: String = s.chars().take(max - ELLIPSIS.len()).collect();
    format!("{kept}{ELLIPSIS}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Signature;
    use crate::view_state::DetailSel;

    fn sig(name: &str, email: &str, when: &str) -> Signature {
        Signature { name: name.to_string(), email: email.to_string(), when: when.to_string() }
    }

    fn detail(subject: &str) -> CommitDetail {
        CommitDetail {
            subject: subject.to_string(),
            short_hash: "abc1234".to_string(),
            author: sig("Ada Lovelace", "ada@example.com", "01.01.2026, 00:00"),
            committer: sig("Ada Lovelace", "ada@example.com", "01.01.2026, 00:00"),
            containing_branches: vec![],
            working: None,
        }
    }

    fn area(w: u16, h: u16) -> Rect {
        Rect { x: 0, y: 0, width: w, height: h }
    }

    /// The subject is the second visual line (row 1): a leading blank, then the band.
    #[test]
    fn wide_pane_does_not_wrap_the_subject() {
        let d = detail("HELLO WORLD");
        let view = ViewState::new(0);
        let lines = wrapped_lines(&d, &view, area(40, 20));
        assert_eq!(line_text(&lines[1]).trim_end(), "  HELLO WORLD");
    }

    #[test]
    fn narrow_pane_word_wraps_the_subject_at_a_space() {
        let d = detail("alpha beta gamma");
        let view = ViewState::new(0);
        // INDENT(2) + "alpha beta gamma" = 18 cols; a width of 12 forces a fold.
        let lines = wrapped_lines(&d, &view, area(12, 20));
        // Row 1 holds the first chunk; it must break at a space, not mid-word.
        let first = line_text(&lines[1]);
        assert!(first.trim_end() == "  alpha beta", "got {first:?}");
        // The remainder folds onto the next row WITH the hanging indent (the leading
        // padding is repeated, not dropped to column 0).
        assert_eq!(line_text(&lines[2]).trim_end(), "  gamma");
    }

    #[test]
    fn a_token_longer_than_the_width_hard_breaks() {
        let d = detail("supercalifragilistic");
        let view = ViewState::new(0);
        let lines = wrapped_lines(&d, &view, area(10, 20));
        // No space to break on -> hard cut, but each row keeps the 2-col hanging indent,
        // so the content budget is width - indent = 8 chars per row.
        assert_eq!(line_text(&lines[1]), "  supercal");
        assert_eq!(line_text(&lines[2]).trim_end(), "  ifragili");
        assert_eq!(line_text(&lines[3]).trim_end(), "  stic");
    }

    #[test]
    fn word_span_covers_the_clicked_word() {
        let d = detail("HELLO WORLD");
        let view = ViewState::new(0);
        // Row 1 = "  HELLO WORLD"; col 4 lands inside HELLO (chars 2..7).
        let (row, start, end) = word_span(&d, &view, area(40, 20), 1, 4).unwrap();
        assert_eq!((row, start, end), (1, 2, 7));
    }

    #[test]
    fn selected_text_slices_the_span() {
        let d = detail("HELLO WORLD");
        let mut view = ViewState::new(0);
        view.detail_sel = Some(DetailSel { anchor: (1, 2), cursor: (1, 7) });
        assert_eq!(selected_text(&d, &view, area(40, 20)).as_deref(), Some("HELLO"));
    }

    #[test]
    fn selected_text_spans_multiple_rows() {
        let d = detail("alpha beta gamma");
        let mut view = ViewState::new(0);
        // Wrapped at width 12 with a hanging indent: row1 "  alpha beta", row2
        // "  gamma". Select from row1 col 8 ("beta") through row2 col 7 (end of the
        // indented "gamma"). Copy strips row2's SYNTHETIC hanging indent (render padding,
        // not source) -> the source text "beta\ngamma", not "beta\n  gamma".
        view.detail_sel = Some(DetailSel { anchor: (1, 8), cursor: (2, 7) });
        assert_eq!(selected_text(&d, &view, area(12, 20)).as_deref(), Some("beta\ngamma"));
    }

    #[test]
    fn click_below_content_clamps_to_the_last_line() {
        let d = detail("hi");
        let view = ViewState::new(0);
        let lines = wrapped_lines(&d, &view, area(40, 20));
        // A click far below the content lands on the last visual row, clamped col.
        let (r, _c) = click_pos(&d, &view, area(40, 20), 5, 200).unwrap();
        assert_eq!(r, lines.len() - 1);
    }

    #[test]
    fn empty_selection_yields_no_text() {
        let d = detail("hi");
        let mut view = ViewState::new(0);
        view.detail_sel = Some(DetailSel { anchor: (1, 3), cursor: (1, 3) });
        assert_eq!(selected_text(&d, &view, area(40, 20)), None);
    }

    #[test]
    fn working_detail_shows_uncommitted_summary_with_branch_not_the_subject() {
        use crate::model::WorkingSummary;
        let mut d = detail("a commit subject that must NOT appear");
        d.working = Some(WorkingSummary {
            branch: Some("main".to_string()),
            added: 1,
            changed: 23,
            deleted: 0,
        });
        let view = ViewState::new(0);
        let text = wrapped_lines(&d, &view, area(60, 20))
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Uncommitted changes"), "header present: {text:?}");
        assert!(text.contains("On branch main"), "current branch shown: {text:?}");
        // Order: Changed first, then Added, then Deleted.
        let (chg, add, del) = (
            text.find("Changed:").unwrap(),
            text.find("Added:").unwrap(),
            text.find("Deleted:").unwrap(),
        );
        assert!(chg < add && add < del, "counts ordered Changed/Added/Deleted: {text:?}");
        assert!(text.contains("23"), "the changed count is rendered");
        assert!(!text.contains("a commit subject"), "the working row hides the commit subject");

        // A zero count is dimmed (no status color); a nonzero count takes the status color.
        let lines = wrapped_lines(&d, &view, area(60, 20));
        let num_fg = |label: &str| -> ratatui::style::Color {
            let line = lines.iter().find(|l| line_text(l).contains(label)).expect("count line");
            line.spans.last().expect("number span").style.fg.expect("number fg")
        };
        assert_eq!(num_fg("Deleted:"), Theme::TEXT_DIM, "deleted 0 is dimmed");
        assert_eq!(num_fg("Changed:"), status_color(FileStatus::Modified), "changed 23 is blue");
    }

    #[test]
    fn branch_link_rect_tracks_the_wrapped_header_row() {
        // A long subject + a containing branch: in a wide pane nothing wraps, so the
        // "Hide" link sits at its logical row; in a narrow pane the subject folds and
        // pushes the branch header (and its clickable link) DOWN by the wrap height. The
        // rect must follow render, else a click lands on the wrong row.
        let mut d = detail("a very long subject that will wrap across several visual rows when narrow");
        d.containing_branches = vec!["main".to_string()];
        let mut view = ViewState::new(0);
        view.branches_expanded = true; // expanded always shows the "Hide" link
        let wide = branches_link_rect(&d, &view, area(120, 40)).expect("link in wide pane");
        let narrow = branches_link_rect(&d, &view, area(24, 40)).expect("link in narrow pane");
        assert!(
            narrow.y > wide.y,
            "the link rect tracks the wrapped header (narrow {} must exceed wide {})",
            narrow.y,
            wide.y
        );
    }
}
