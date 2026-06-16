//! Diff/preview viewer (top region).
//!
//! Renders ONE [`FileDiff`] in either side-by-side or unified mode, plus a
//! single-pane source preview for unchanged files. Side-by-side vs unified is a
//! pure render transform over the same `Vec<DiffLine>` (no data duplication):
//! [`pair_rows`] aligns Removed/Added pairs onto two columns, while unified emits
//! one column with a +/-/space sign. Change bands, inline highlights, gutter
//! line-numbers, word-wrap, whitespace markers and vertical scroll are all
//! honored here. PURE: takes the model + layout, never imports store/Msg.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};
use ratatui::Frame;

use super::layout::{list_offset, DiffLayout};
use super::log_panel::abbrev_author;
use super::widgets::{centered_notice, str_width, truncate};
use crate::diff::{BinaryFile, BlameFile, BlameLine, DiffLine, FileDiff, FileView, LineKind, SourceFile, Token};
use crate::highlight::{highlight_line, lang_of};
use crate::model::Status;
use crate::theme::{Glyph, Theme};
use crate::view_state::{EditorState, Pane, ViewState};

/// Width of the line-number gutter (digits + one trailing space), in columns.
const GUTTER_W: u16 = 5;

/// Map a mouse click at `(col, row)` to a 0-based buffer `(line, col)` on the
/// EDITABLE new side, or `None` when the click is outside the editable text area.
/// Pure: mirrors the render's pairing + free/cursor scroll (via [`with_edit_scroll`]) so
/// a click lands on the cell the user sees. Used by the runtime to place the cursor /
/// extend a selection. A fold marker (git's omitted context) is a normal row in `d.lines`,
/// so it needs no special click offset - the editable `<current>` diff carries none anyway.
/// The editable code area's horizontal cell range `(left_code_x, right_edge_x)` for the
/// NO-WRAP render: the first code column (past the gutter) and the last paintable column.
/// The runtime uses it to detect a drag at the pane edge for auto-horizontal-scroll. `None`
/// under word-wrap (text wraps, no horizontal axis) or when the body is too narrow.
pub fn editable_code_x_range(dl: &DiffLayout, view: &ViewState) -> Option<(u16, u16)> {
    if view.word_wrap {
        return None;
    }
    let body = if view.diff_full_width {
        full_width_body(dl)
    } else if let Some(bn) = dl.body_new {
        bn
    } else {
        dl.body_old
    };
    (body.width > GUTTER_W).then(|| (body.x + GUTTER_W, body.right().saturating_sub(1)))
}

pub fn locate_edit_click(
    dl: &DiffLayout,
    d: &FileDiff,
    editor: &crate::view_state::EditorState,
    view: &ViewState,
    col: u16,
    row: u16,
) -> Option<(usize, usize)> {
    let cursor_line = editor.cursor_row + 1;
    let lang = lang_of(&d.path);
    let ctx = EditCtx { cursor: (editor.cursor_row, editor.cursor_col), sel: editor.selection(), lang: &lang };
    let hit = |body: Rect| row >= body.y && row < body.bottom() && col >= body.x && col < body.right();
    // The bottom body row is the horizontal scrollbar (when present); shrink the bodies
    // so a click on the track is not read as the last buffer line.
    let bar = diff_hbar(dl, d, view, Some(editor));
    // No-change file: the editable buffer is ONE full-width pane (single gutter), so the
    // click maps against the full-width body, not the right half. Mirrors the new-side
    // scroll derivation in `render_full_width` (height = the new side alone, no pairing).
    if view.diff_full_width {
        let body = body_above_hbar(full_width_body(dl), &bar);
        if !hit(body) {
            return None;
        }
        let (_old_rows, new_rows) = pair_rows(d);
        let focus_row = new_rows.iter().position(|r| line_new_no(r) == Some(cursor_line));
        let click_y = (row - body.y) as usize;
        let code_w = body.width.saturating_sub(GUTTER_W) as usize;
        if view.word_wrap {
            let height = |i: usize| {
                let new_edit = line_new_no(&new_rows[i]).map(|n| (n - 1, ctx));
                row_code_height(&new_rows[i], view, body.width, new_edit, Some(&lang))
            };
            let window = body.height as usize;
            let follow = wrapped_scroll(focus_row.unwrap_or(0), window, new_rows.len(), &height);
            let scroll = with_edit_scroll(view, true, new_rows.len(), follow);
            let cy = click_y;
            let (i, sub) = wrapped_index_at(scroll, new_rows.len(), cy, &height)?;
            let bl = line_new_no(&new_rows[i])?.checked_sub(1)?;
            return Some((bl, wrapped_col(d, bl, col, body.x, GUTTER_W, sub, code_w)));
        }
        let follow = list_offset(focus_row.unwrap_or(0), new_rows.len(), body.height as usize);
        let scroll = with_edit_scroll(view, true, new_rows.len(), follow);
        let nrow = new_rows.get(click_y + scroll)?;
        let bl = line_new_no(nrow)?.checked_sub(1)?;
        let hoff = nowrap_hscroll(view, d, Some(ctx.cursor.1), code_w);
        return Some((bl, click_col(d, bl, col, body.x, GUTTER_W, hoff)));
    }
    match dl.body_new.map(|b| body_above_hbar(b, &bar)) {
        // Side-by-side: the new (right) pane carries the editable buffer.
        Some(body) if hit(body) => {
            let (old_rows, new_rows) = pair_rows(d);
            let focus_row = new_rows.iter().position(|r| line_new_no(r) == Some(cursor_line));
            let click_y = (row - body.y) as usize;
            let code_w = body.width.saturating_sub(GUTTER_W) as usize;
            if view.word_wrap {
                // Use the SAME per-pair max heights + physical-aware scroll the render
                // uses, so the click resolves the logical row + sub-row that was painted.
                let height = |i: usize| {
                    let new_edit = line_new_no(&new_rows[i]).map(|n| (n - 1, ctx));
                    let oh = row_code_height(&old_rows[i], view, dl.body_old.width, None, Some(&lang));
                    let nh = row_code_height(&new_rows[i], view, body.width, new_edit, Some(&lang));
                    oh.max(nh)
                };
                let window = body.height as usize;
                let follow = wrapped_scroll(focus_row.unwrap_or(0), window, new_rows.len(), &height);
                let scroll = with_edit_scroll(view, true, new_rows.len(), follow);
                let cy = click_y;
                let (i, sub) = wrapped_index_at(scroll, new_rows.len(), cy, &height)?;
                let bl = line_new_no(&new_rows[i])?.checked_sub(1)?;
                Some((bl, wrapped_col(d, bl, col, body.x, GUTTER_W, sub, code_w)))
            } else {
                let follow = list_offset(focus_row.unwrap_or(0), new_rows.len(), body.height as usize);
                let scroll = with_edit_scroll(view, true, new_rows.len(), follow);
                let nrow = new_rows.get(click_y + scroll)?;
                let bl = line_new_no(nrow)?.checked_sub(1)?;
                let hoff = nowrap_hscroll(view, d, Some(ctx.cursor.1), code_w);
                Some((bl, click_col(d, bl, col, body.x, GUTTER_W, hoff)))
            }
        }
        // Unified: the single column; only lines that carry a new_no are editable.
        None => {
            let body = body_above_hbar(dl.body_old, &bar);
            if !hit(body) {
                return None;
            }
            let prefix_w = GUTTER_W * 2 + 2; // two gutters + the sign column
            let code_w = body.width.saturating_sub(prefix_w) as usize;
            let window = body.height as usize;
            let cur_idx = d.lines.iter().position(|l| l.new_no == Some(cursor_line)).unwrap_or(0);
            let click_y = (row - body.y) as usize;
            if view.word_wrap {
                let height = |i: usize| {
                    let edit = d.lines[i].new_no.map(|n| (n - 1, ctx));
                    row_code_height_line(&d.lines[i], view, body.width, prefix_w, edit, Some(&lang))
                };
                let follow = wrapped_scroll(cur_idx, window, d.lines.len(), &height);
                let scroll = with_edit_scroll(view, true, d.lines.len(), follow);
                let cy = click_y;
                let (i, sub) = wrapped_index_at(scroll, d.lines.len(), cy, &height)?;
                let bl = d.lines[i].new_no?.checked_sub(1)?;
                Some((bl, wrapped_col(d, bl, col, body.x, prefix_w, sub, code_w)))
            } else {
                let follow = list_offset(cur_idx, d.lines.len(), window);
                let scroll = with_edit_scroll(view, true, d.lines.len(), follow);
                let line = d.lines.get(click_y + scroll)?;
                let bl = line.new_no?.checked_sub(1)?;
                let hoff = nowrap_hscroll(view, d, Some(ctx.cursor.1), code_w);
                Some((bl, click_col(d, bl, col, body.x, prefix_w, hoff)))
            }
        }
        _ => None,
    }
}

/// Logical scroll start that keeps logical row `focus` visible in a `body_h`-PHYSICAL-row
/// viewport when rows word-wrap, given each row's wrapped physical `height`. Counts the
/// focus row's own height, then includes earlier rows while they still fit, so the focus
/// row's bottom sits within the viewport - the physical-row analogue of `list_offset`
/// (which assumes height 1 and would let the caret scroll off the bottom under wrap). A
/// focus row taller than the whole viewport is shown from its top.
fn wrapped_scroll(focus: usize, body_h: usize, n: usize, height: &impl Fn(usize) -> usize) -> usize {
    if n == 0 || body_h == 0 {
        return 0;
    }
    let focus = focus.min(n - 1);
    let mut acc = height(focus).max(1);
    let mut start = focus;
    while start > 0 {
        let h = height(start - 1).max(1);
        if acc + h > body_h {
            break;
        }
        acc += h;
        start -= 1;
    }
    start
}

/// The editable diff's effective scroll top: the user's free-scroll override
/// (`view.edit_scroll`, a mouse-wheel tick that scrolls WITHOUT moving the caret) when
/// `editing`, else the cursor-follow `follow` offset. The override is clamped to the
/// last row so a stale value (e.g. after a resize) can never blank the pane. The renderer
/// AND the click hit-test both route their per-regime `follow` through this ONE function,
/// so a free-scrolled view maps a click to the same cell it painted (the render==hit-test
/// invariant). When not editing (browsing) the override is ignored, so the golden render
/// is unaffected.
fn with_edit_scroll(view: &ViewState, editing: bool, n: usize, follow: usize) -> usize {
    match view.edit_scroll {
        Some(top) if editing => top.min(n.saturating_sub(1)),
        _ => follow,
    }
}

/// The editable diff's current scroll `top` and its maximum, for the active regime
/// (side-by-side vs unified, wrapped vs flat). A mouse-wheel tick reads this to nudge
/// the viewport by whole rows WITHOUT moving the caret: `top` is the free-scroll override
/// when set (clamped), else the cursor-follow offset; `max` is the largest top that still
/// shows content (the last logical row pinned to the bottom). `None` when there is no
/// body / no rows. The runtime clamps `top + delta` to `[0, max]` and sends the editable
/// scroll message. Mirrors [`locate_edit_click`]'s scroll derivation so the wheel, the
/// render and the hit-test all agree on the offset.
pub fn edit_scroll_bounds(
    dl: &DiffLayout,
    d: &FileDiff,
    editor: &EditorState,
    view: &ViewState,
) -> Option<(usize, usize)> {
    let lang = lang_of(&d.path);
    let ctx = EditCtx {
        cursor: (editor.cursor_row, editor.cursor_col),
        sel: editor.selection(),
        lang: &lang,
    };
    let cursor_line = editor.cursor_row + 1;
    match dl.body_new {
        // Side-by-side: rows are paired; the new side carries the caret.
        Some(body) => {
            let (old_rows, new_rows) = pair_rows(d);
            let n = new_rows.len();
            if n == 0 {
                return None;
            }
            let focus = new_rows
                .iter()
                .position(|r| line_new_no(r) == Some(cursor_line))
                .unwrap_or(0);
            if view.word_wrap {
                let height = |i: usize| {
                    let ne = line_new_no(&new_rows[i]).map(|nn| (nn - 1, ctx));
                    row_code_height(&old_rows[i], view, dl.body_old.width, None, Some(&lang))
                        .max(row_code_height(&new_rows[i], view, body.width, ne, Some(&lang)))
                };
                let window = body.height as usize;
                let max = wrapped_scroll(n - 1, window, n, &height);
                let cur = view
                    .edit_scroll
                    .map_or_else(|| wrapped_scroll(focus, window, n, &height), |t| t.min(max));
                Some((cur, max))
            } else {
                let window = dl.body_old.height as usize;
                let max = list_offset(n - 1, n, window);
                let cur = view
                    .edit_scroll
                    .map_or_else(|| list_offset(focus, n, window), |t| t.min(max));
                Some((cur, max))
            }
        }
        // Unified: the single signed column; only `new_no` lines are editable.
        None => {
            let body = dl.body_old;
            let n = d.lines.len();
            if n == 0 {
                return None;
            }
            let prefix_w = GUTTER_W * 2 + 2;
            let focus = d
                .lines
                .iter()
                .position(|l| l.new_no == Some(cursor_line))
                .unwrap_or(0);
            let window = body.height as usize;
            if view.word_wrap {
                let height = |i: usize| {
                    let ei = d.lines[i].new_no.map(|nn| (nn - 1, ctx));
                    row_code_height_line(&d.lines[i], view, body.width, prefix_w, ei, Some(&lang))
                };
                let max = wrapped_scroll(n - 1, window, n, &height);
                let cur = view
                    .edit_scroll
                    .map_or_else(|| wrapped_scroll(focus, window, n, &height), |t| t.min(max));
                Some((cur, max))
            } else {
                let max = list_offset(n - 1, n, window);
                let cur = view
                    .edit_scroll
                    .map_or_else(|| list_offset(focus, n, window), |t| t.min(max));
                Some((cur, max))
            }
        }
    }
}

/// The widest code line in `d`, in CHAR columns (one cell per char, tabs/spaces
/// included). The horizontal-scroll ceiling: an offset past `longest - code_w` would
/// scroll the whole line off the code column into blank space.
fn longest_line(d: &FileDiff) -> usize {
    d.lines
        .iter()
        .map(|l| l.tokens.iter().map(|t| t.text.chars().count()).sum::<usize>())
        .max()
        .unwrap_or(0)
}

/// The diff's effective horizontal offset (code columns hidden off the left) for a
/// NO-WRAP render. The wheel override wins; absent one, an editing caret is kept in the
/// last visible column (cursor-follow) while a browsed diff stays at column 0.
/// `caret_col` is the caret's char column when editing, `None` when browsing.
///
/// Only the manual OVERRIDE is clamped to the longest line, so a stale value (e.g. a
/// `--watch` reload that shrinks the still-selected preview) can never scroll the code
/// fully off into blank cells. The caret-follow offset is NOT clamped - it is computed
/// from the live caret and is always valid (clamping it would hide the trailing cursor
/// cell at end-of-line, which sits one column past the last char).
fn nowrap_hscroll(view: &ViewState, d: &FileDiff, caret_col: Option<usize>, code_w: usize) -> usize {
    match view.diff_hscroll {
        Some(n) => n.min(longest_line(d).saturating_sub(code_w)),
        None => caret_col.map_or(0, |c| c.saturating_sub(code_w.saturating_sub(1))),
    }
}

/// The diff's current horizontal offset + the max scrollable offset, for the wheel: the
/// runtime clamps `cur + delta` to `[0, max]` and sends the horizontal-scroll message.
/// `None` when word-wrap is on (no sideways scroll) or there is no measurable code area.
/// Reuses the renderer's [`nowrap_hscroll`] for `cur` so the wheel, render and hit-test
/// all agree on the effective offset.
pub fn hscroll_bounds(
    dl: &DiffLayout,
    d: &FileDiff,
    view: &ViewState,
    editor: Option<&EditorState>,
) -> Option<(usize, usize)> {
    if view.word_wrap {
        return None;
    }
    // The code area whose width the offset clamps to MUST match what the renderer paints:
    // a no-change file is one full-width single-gutter pane; otherwise the right pane
    // (side-by-side) or the single column minus the dual gutters + sign (unified).
    let code_w = if view.diff_full_width {
        full_width(dl).saturating_sub(GUTTER_W) as usize
    } else {
        match dl.body_new {
            Some(body) => body.width.saturating_sub(GUTTER_W) as usize,
            None => dl.body_old.width.saturating_sub(GUTTER_W * 2 + 2) as usize,
        }
    };
    if code_w == 0 {
        return None;
    }
    let max = longest_line(d).saturating_sub(code_w);
    let caret_col = editor.filter(|e| e.loaded).map(|e| e.cursor_col);
    let cur = nowrap_hscroll(view, d, caret_col, code_w);
    Some((cur, max))
}

/// The diff's horizontal scrollbar: the track row (the BOTTOM row of the body region,
/// reserved when present), the current/max offset, and the longest-line width - shared
/// by the renderer and the drag hit-test so the thumb and the click agree cell-for-cell.
/// `None` when word-wrap is on, nothing overflows the pane, or the body is too short to
/// spare a row. Only the bottom row is consumed; the code rows render above it.
struct DiffHBar {
    track: Rect,
    cur: usize,
    max: usize,
    content_w: usize,
}

/// Compute the [`DiffHBar`] for the current diff, or `None` when there is nothing to
/// scroll sideways (wrap on / no overflow / body < 2 rows).
fn diff_hbar(dl: &DiffLayout, d: &FileDiff, view: &ViewState, editor: Option<&EditorState>) -> Option<DiffHBar> {
    let (cur, max) = hscroll_bounds(dl, d, view, editor)?;
    if max == 0 || dl.body_old.height < 2 {
        return None;
    }
    let track = Rect { x: dl.body_old.x, y: dl.body_old.bottom() - 1, width: full_width(dl), height: 1 };
    Some(DiffHBar { track, cur, max, content_w: longest_line(d) })
}

/// `body` with its last row reserved for the scrollbar (when one is present), so the code
/// rows never paint over the track. Both the renderer and the hit-test shrink through
/// this, keeping the click->cell map exact.
fn body_above_hbar(body: Rect, bar: &Option<DiffHBar>) -> Rect {
    match bar {
        Some(_) => Rect { height: body.height.saturating_sub(1), ..body },
        None => body,
    }
}

/// The thumb's `(x, width)` within the track, proportional to the visible window over the
/// longest line. At least one cell wide; pinned to the track's right edge at `cur == max`.
fn hbar_thumb(bar: &DiffHBar) -> (u16, u16) {
    let tw = bar.track.width.max(1) as usize;
    let content = bar.content_w.max(1);
    let visible = content.saturating_sub(bar.max).max(1); // code columns on screen
    let thumb_w = (tw * visible / content).clamp(1, tw);
    let travel = tw.saturating_sub(thumb_w);
    let off = (travel * bar.cur).checked_div(bar.max).unwrap_or(0);
    (bar.track.x + off as u16, thumb_w as u16)
}

/// The offset a thumb whose LEFT edge sits at `thumb_x` maps to - the inverse of
/// [`hbar_thumb`]'s placement (so a grab keeps the thumb under the pointer instead of
/// snapping its left edge to the click column). Scaled over the thumb's TRAVEL, not the
/// full track, so the math agrees with the rendered thumb.
fn hbar_offset_for_thumb_x(bar: &DiffHBar, thumb_x: u16) -> usize {
    let (_, thumb_w) = hbar_thumb(bar);
    let tw = bar.track.width.max(1) as usize;
    let travel = tw.saturating_sub(thumb_w as usize);
    if travel == 0 {
        return 0;
    }
    let rel = thumb_x.saturating_sub(bar.track.x).min(travel as u16) as usize;
    (rel * bar.max / travel).min(bar.max)
}

/// A press on the track: the `(offset, grab_dx)` to scroll to and the grab point WITHIN
/// the thumb (so a following drag tracks relative to where it was grabbed). A press ON the
/// thumb does NOT jump (offset stays `cur`); a press on the bare track centers the thumb
/// under the pointer. Fixes the "re-click jumps" defect where the click column was mapped
/// straight to an offset, ignoring the thumb's width.
fn hbar_press_at(bar: &DiffHBar, col: u16) -> (usize, u16) {
    let (tx, tw) = hbar_thumb(bar);
    if col >= tx && col < tx + tw {
        return (bar.cur, col - tx);
    }
    let grab = tw / 2;
    (hbar_offset_for_thumb_x(bar, col.saturating_sub(grab)), grab)
}

/// Paint the scrollbar: a faint recessed track with a brighter thumb over it, drawn with
/// a LOWER HALF block so the bar is half a row tall (a thin bar hugging the bottom) while
/// the full row stays clickable. The unfilled top half shows the code canvas behind it.
fn render_diff_hbar(frame: &mut Frame, bar: &DiffHBar) {
    let bar_span = |w: u16, fg: Color| {
        Span::styled(
            Glyph::SCROLL_BAR.to_string().repeat(w as usize),
            Style::default().fg(fg).bg(Theme::CODE_BG),
        )
    };
    frame.render_widget(Paragraph::new(Line::from(bar_span(bar.track.width, Theme::SCROLL_TRACK))), bar.track);
    let (x, width) = hbar_thumb(bar);
    frame.render_widget(
        Paragraph::new(Line::from(bar_span(width, Theme::SCROLL_THUMB))),
        Rect { x, width, ..bar.track },
    );
}

/// If the press/drag at `(col, row)` lands on the diff's horizontal scrollbar track, the
/// new horizontal offset to scroll to (else `None`). The runtime's mouse handler uses it
/// for click-to-jump + thumb drag; `editor` is the open editable buffer (its caret seeds
/// the offset when no manual override is set), or `None` for a read-only diff.
pub fn locate_diff_hbar(
    dl: &DiffLayout,
    d: &FileDiff,
    view: &ViewState,
    editor: Option<&EditorState>,
    col: u16,
    row: u16,
) -> Option<(usize, u16)> {
    let bar = diff_hbar(dl, d, view, editor)?;
    let on_track = row == bar.track.y && col >= bar.track.x && col < bar.track.right();
    on_track.then(|| hbar_press_at(&bar, col))
}

/// The horizontal offset for a CONTINUING scrollbar drag at column `col`, keeping the
/// thumb's grab point `grab_dx` under the pointer - the row is ignored, so the offset still
/// tracks horizontal motion when the cursor drifts off the track row mid-drag. `None` when
/// there is no scrollbar.
pub fn diff_hbar_drag_offset(
    dl: &DiffLayout,
    d: &FileDiff,
    view: &ViewState,
    editor: Option<&EditorState>,
    col: u16,
    grab_dx: u16,
) -> Option<usize> {
    diff_hbar(dl, d, view, editor).map(|bar| hbar_offset_for_thumb_x(&bar, col.saturating_sub(grab_dx)))
}

/// Walk wrapped physical rows from logical `scroll`, returning the `(logical index,
/// sub-row)` containing physical offset `target` (0-based from the first content row).
/// `height(i)` is logical row `i`'s wrapped physical height. `None` past the content.
fn wrapped_index_at(scroll: usize, len: usize, target: usize, height: &impl Fn(usize) -> usize) -> Option<(usize, usize)> {
    let mut acc = 0usize;
    for i in scroll..len {
        let h = height(i).max(1);
        if target < acc + h {
            return Some((i, target - acc));
        }
        acc += h;
    }
    None
}

/// The char column a click maps to on a WRAPPED line: the click's column within the
/// code area, added to the char index where the clicked physical sub-row STARTS. The
/// sub-row start is computed with the SAME word-wrap walk the renderer uses (via
/// [`wrap_row_starts`]), so it is exact at word boundaries - not the uniform
/// `sub * code_w` estimate. Clamped to the buffer line's length.
fn wrapped_col(d: &FileDiff, bl: usize, col: u16, body_x: u16, prefix_w: u16, sub: usize, code_w: usize) -> usize {
    let in_code = col.saturating_sub(body_x + prefix_w) as usize;
    let Some(line) = d.lines.iter().find(|l| l.new_no == Some(bl + 1)) else {
        return in_code;
    };
    let text: String = line.tokens.iter().map(|t| t.text.as_str()).collect();
    let starts = wrap_row_starts(&text, code_w);
    let row_start = starts.get(sub).copied().unwrap_or_else(|| *starts.last().unwrap_or(&0));
    (row_start + in_code).min(text.chars().count())
}

/// The char index at which each physical row STARTS when `text` word-wraps into
/// `width`-wide rows. Mirrors [`wrapped_rows`]/[`split_keep_spaces`] exactly (wrap at
/// word boundaries, hard-break words longer than `width`), so the click->column map
/// agrees with what the renderer paints. Always begins `[0, ...]`.
fn wrap_row_starts(text: &str, width: usize) -> Vec<usize> {
    let mut starts = vec![0usize];
    if width == 0 {
        return starts;
    }
    let mut col = 0usize;
    let mut idx = 0usize; // char index of the running position
    for word in split_keep_spaces(text) {
        let len = word.chars().count();
        if col + len > width && col > 0 {
            starts.push(idx); // this word is pushed wholesale to a new row
            col = 0;
        }
        col += len;
        idx += len;
        // Hard-break a word longer than `width`: a row boundary every `width` chars.
        while col > width {
            starts.push(idx - (col - width));
            col -= width;
        }
    }
    starts
}

/// The char column a click at `col` lands on for buffer line `bl`, given the pane
/// `body_x` and the `prefix` width before the code. Clamped to the line length.
fn click_col(d: &FileDiff, bl: usize, col: u16, body_x: u16, prefix: u16, hoff: usize) -> usize {
    let text_x = body_x + prefix;
    let raw = col.saturating_sub(text_x) as usize + hoff;
    let len = d
        .lines
        .iter()
        .find(|l| l.new_no == Some(bl + 1))
        .map(|l| l.tokens.iter().map(|t| t.text.chars().count()).sum())
        .unwrap_or(0);
    raw.min(len)
}

/// Whether a non-empty read-only text selection is currently active. When it is, the
/// diff-cursor's full-row focus band is suppressed so the per-character selection band
/// is the only highlight (two competing full-row bands read as a broken selection).
fn diff_sel_active(view: &ViewState) -> bool {
    view.diff_sel.is_some_and(|s| !s.is_empty())
}

/// The CHARACTER range `[start, end)` selected on logical diff line `idx` (length
/// `line_len`), or `None` when the line is outside the selection / the selection is
/// empty. Mirrors a real editor: the first line runs from its anchor column to its end,
/// interior lines are fully selected, the last line runs from its start to the cursor
/// column. Half-open columns, clamped to the line length.
fn diff_sel_range(view: &ViewState, idx: usize, line_len: usize) -> Option<(usize, usize)> {
    let sel = view.diff_sel.filter(|s| !s.is_empty())?;
    let ((sr, sc), (er, ec)) = sel.span();
    if idx < sr || idx > er {
        return None;
    }
    let start = if idx == sr { sc } else { 0 };
    let end = if idx == er { ec } else { line_len };
    let (start, end) = (start.min(line_len), end.min(line_len));
    (start < end).then_some((start, end))
}

/// The character count of a diff line's full text (the column bound for a selection).
fn line_char_len(line: &DiffLine) -> usize {
    line.tokens.iter().map(|t| t.text.chars().count()).sum()
}

/// The committed text spanned by the read-only diff selection, CHARACTER-exact: each
/// selected line sliced to its selected columns (first line from the anchor column,
/// interior lines whole, last line up to the cursor column), joined by `\n` for the
/// system clipboard. `None` when there is no selection, it is empty (a bare click), or
/// the diff is empty.
pub fn selected_text(d: &FileDiff, view: &ViewState) -> Option<String> {
    let sel = view.diff_sel.filter(|s| !s.is_empty())?;
    let ((sr, sc), (er, ec)) = sel.span();
    let last = d.lines.len().checked_sub(1)?;
    let (sr, er) = (sr.min(last), er.min(last));
    let slices: Vec<String> = (sr..=er)
        // A fold marker carries no real text - never include it in copied output.
        .filter(|&i| d.lines[i].fold.is_none())
        .map(|i| {
            let text: String = d.lines[i].tokens.iter().map(|t| t.text.as_str()).collect();
            let len = text.chars().count();
            let start = if i == sr { sc.min(len) } else { 0 };
            let end = if i == er { ec.min(len) } else { len };
            text.chars().skip(start).take(end.saturating_sub(start)).collect()
        })
        .collect();
    Some(slices.join("\n"))
}

/// Read-only code spans for a SELECTED diff line: rendered char-by-char (token colors
/// preserved) with the chars in `[sel.0, sel.1)` on the selection band. Mirrors
/// [`edit_line_spans`] minus the caret, so a read-only selection bands exactly like the
/// editable side. Only called for lines that actually carry a selection (the unselected
/// lines keep the cheaper token-run [`code_spans`]).
fn readonly_sel_spans(
    line: &DiffLine,
    hl: Option<&str>,
    show_ws: bool,
    row_bg: Color,
    sel: (usize, usize),
) -> Vec<Span<'static>> {
    if let Some(hidden) = line.fold {
        // A fold marker carries no selectable text - render the dim summary, ignoring the
        // (empty) selection range so the marker never vanishes mid drag-select.
        return fold_marker_spans(hidden);
    }
    let text: String = line.tokens.iter().map(|t| t.text.as_str()).collect();
    let chars: Vec<char> = text.chars().collect();
    // One syntax color per char: on-the-fly highlight for the live buffer (`hl`), else
    // the read-only diff's pre-tokenized kinds expanded to one color per char.
    let mut fg: Vec<Color> = Vec::with_capacity(chars.len());
    match hl {
        Some(lang) => {
            for tok in highlight_line(lang, &text) {
                fg.extend(std::iter::repeat_n(tok.kind.color(), tok.text.chars().count()));
            }
        }
        None => {
            for tok in &line.tokens {
                fg.extend(std::iter::repeat_n(tok.kind.color(), tok.text.chars().count()));
            }
        }
    }
    let inline = line.inline_hl.map(|range| (range, inline_band(line.kind)));
    let mut spans = Vec::with_capacity(chars.len());
    for (col, ch) in chars.iter().enumerate() {
        let ws = is_ws_marker(*ch, show_ws);
        let color = marker_fg(fg.get(col).copied().unwrap_or(Theme::TEXT), ws);
        let style = if col >= sel.0 && col < sel.1 {
            Style::default().bg(Theme::SELECTION_EDIT).fg(color)
        } else if char_in_inline(col, inline) {
            Style::default().bg(inline_band(line.kind)).fg(color).bold()
        } else {
            Style::default().bg(row_bg).fg(color)
        };
        spans.push(Span::styled(render_char(*ch, show_ws).to_string(), style));
    }
    spans
}

/// The `(LOGICAL diff-line index, CHARACTER column)` under a click on a READ-ONLY diff
/// body (either pane in side-by-side, or the single unified column), resolved with the
/// SAME browse scroll + horizontal offset the renderer used so the click maps to the
/// cell it drew. `None` over a filler row, outside any body, or under word-wrap when the
/// click is past the content. Drives the character-level read-only selection; the
/// editable buffer uses [`locate_edit_click`] instead.
pub fn locate_diff_click(dl: &DiffLayout, d: &FileDiff, view: &ViewState, col: u16, row: u16) -> Option<(usize, usize)> {
    // Mirror the renderer: a fold-marker row never carries the focus band, so the
    // browse scroll must not treat the cursor as focused there either.
    let focus_idx = (view.focus == Pane::Diff)
        .then_some(view.diff_cursor)
        .filter(|&c| d.lines.get(c).is_none_or(|l| l.fold.is_none()));
    let hit = |b: Rect| row >= b.y && row < b.bottom() && col >= b.x && col < b.right();
    // The bottom body row is the horizontal scrollbar (when present) - shrink the bodies
    // so a click on the track never resolves to a phantom last line.
    let bar = diff_hbar(dl, d, view, None);
    let body_old = body_above_hbar(dl.body_old, &bar);
    match dl.body_new.map(|b| body_above_hbar(b, &bar)) {
        // Side-by-side: a click on EITHER pane resolves the logical line it carries.
        Some(body_new) => {
            let (old_rows, new_rows) = pair_rows(d);
            let body = if hit(body_old) {
                body_old
            } else if hit(body_new) {
                body_new
            } else {
                return None;
            };
            let rows = if body.x == body_old.x { &old_rows } else { &new_rows };
            let body_h = body_old.height as usize;
            let code_w = body.width.saturating_sub(GUTTER_W) as usize;
            let focus_row = focus_idx.and_then(|c| {
                old_rows
                    .iter()
                    .position(|r| r.idx == Some(c))
                    .or_else(|| new_rows.iter().position(|r| r.idx == Some(c)))
            });
            let click_y = (row - body.y) as usize;
            if view.word_wrap {
                let height = |i: usize| {
                    row_code_height(&old_rows[i], view, body_old.width, None, None)
                        .max(row_code_height(&new_rows[i], view, body_new.width, None, None))
                };
                let window = body_h;
                let scroll = match focus_row {
                    Some(fr) => with_edit_scroll(view, false, old_rows.len(), wrapped_scroll(fr, window, old_rows.len(), &height)),
                    None => view.diff_scroll,
                };
                let cy = click_y;
                let (i, sub) = wrapped_index_at(scroll, old_rows.len(), cy, &height)?;
                let line = rows[i].idx?;
                Some((line, readonly_wrapped_col(d, line, col, body.x, GUTTER_W, sub, code_w)))
            } else {
                let scroll = focused_scroll(view, false, focus_row, old_rows.len(), body_h);
                let cy = click_y;
                let line = rows.get(cy + scroll)?.idx?;
                // Both panes share ONE horizontal offset (render derives it from the NEW
                // pane), so the hit-test must clamp with the new pane's width too - else an
                // OLD-pane click maps a few columns off when the divider is asymmetric.
                let hoff = nowrap_hscroll(view, d, None, body_new.width.saturating_sub(GUTTER_W) as usize);
                Some((line, readonly_col(d, line, col, body.x + GUTTER_W, hoff)))
            }
        }
        // Unified: the single column; every row is one logical diff line.
        None => {
            let body = body_old;
            if !hit(body) {
                return None;
            }
            let prefix_w = GUTTER_W * 2 + 2;
            let code_w = body.width.saturating_sub(prefix_w) as usize;
            let window = body.height as usize;
            let click_y = (row - body.y) as usize;
            let cy = click_y;
            if view.word_wrap {
                let height = |i: usize| row_code_height_line(&d.lines[i], view, body.width, prefix_w, None, None);
                let scroll = match focus_idx {
                    Some(f) => with_edit_scroll(view, false, d.lines.len(), wrapped_scroll(f, window, d.lines.len(), &height)),
                    None => view.diff_scroll,
                };
                let (i, sub) = wrapped_index_at(scroll, d.lines.len(), cy, &height)?;
                Some((i, readonly_wrapped_col(d, i, col, body.x, prefix_w, sub, code_w)))
            } else {
                let scroll = match focus_idx {
                    Some(f) => with_edit_scroll(view, false, d.lines.len(), list_offset(f, d.lines.len(), window)),
                    None => view.diff_scroll,
                };
                let i = cy + scroll;
                if i >= d.lines.len() {
                    return None;
                }
                let hoff = nowrap_hscroll(view, d, None, code_w);
                Some((i, readonly_col(d, i, col, body.x + prefix_w, hoff)))
            }
        }
    }
}

/// The character column a read-only click at `col` lands on for logical line `i`, given
/// the code area's left edge `text_x` and the horizontal scroll `hoff`. Clamped to the
/// line's length.
fn readonly_col(d: &FileDiff, i: usize, col: u16, text_x: u16, hoff: usize) -> usize {
    let len = d.lines.get(i).map(line_char_len).unwrap_or(0);
    (col.saturating_sub(text_x) as usize + hoff).min(len)
}

/// The character column a read-only click maps to on a WRAPPED line `i`: the click's
/// column within the code area plus the char index where the clicked physical sub-row
/// starts (the same word-wrap walk the renderer uses). Clamped to the line length.
fn readonly_wrapped_col(d: &FileDiff, i: usize, col: u16, body_x: u16, prefix_w: u16, sub: usize, code_w: usize) -> usize {
    let in_code = col.saturating_sub(body_x + prefix_w) as usize;
    let Some(line) = d.lines.get(i) else { return in_code };
    let text: String = line.tokens.iter().map(|t| t.text.as_str()).collect();
    let starts = wrap_row_starts(&text, code_w);
    let row_start = starts.get(sub).copied().unwrap_or_else(|| *starts.last().unwrap_or(&0));
    (row_start + in_code).min(text.chars().count())
}

/// Render the viewer into `dl` for the selected file's `preview`. `file_selected`
/// (computed purely in `super::view` from the tree + selection) distinguishes "a
/// file is selected, its preview is still loading" from "nothing is selected".
pub fn render(
    frame: &mut Frame,
    dl: &DiffLayout,
    preview: Option<&FileView>,
    view: &ViewState,
    status: &Status,
    file_selected: bool,
    inspect: Option<&str>,
) {
    // Dark code canvas across both header and body of the whole diff region.
    frame.render_widget(
        Block::default().style(Style::default().bg(Theme::CODE_BG)),
        canvas_bounds(dl),
    );

    // An inspect overlay renders READ-ONLY (no editable caret) under its own header title.
    let read_only = inspect.is_some();
    match preview {
        Some(FileView::Diff(d)) => render_diff(frame, dl, d, view, read_only, inspect),
        Some(FileView::Source(s)) => render_source(frame, dl, s, view, inspect),
        Some(FileView::Blame(b)) => render_blame(frame, dl, b, view, inspect),
        Some(FileView::Binary(b)) => render_binary(frame, dl, b),
        // A file is selected but its preview has not arrived (and no error): the
        // loader is fetching it - show a transient notice. Otherwise (no selection,
        // or an error) fall back to the existing empty state.
        None if file_selected && !matches!(status, Status::Error(_)) => {
            render_loading(frame, dl)
        }
        None => render_empty(frame, dl),
    }
}

/// Bounding rect spanning the whole diff region (header row + body).
fn canvas_bounds(dl: &DiffLayout) -> Rect {
    Rect {
        x: dl.header_old.x,
        y: dl.header_old.y,
        width: dl.header_old.width
            + dl.divider.map(|d| d.width).unwrap_or(0)
            + dl.body_new.map(|b| b.width).unwrap_or(0),
        height: dl.header_old.height + dl.body_old.height,
    }
}

// -- diff rendering --------------------------------------------------------

/// One visual row on one side: an optional source line plus the band to paint
/// behind it. `None` line = an empty filler row opposite a one-sided change.
struct VisRow<'a> {
    line: Option<&'a DiffLine>,
    band: Option<Color>,
    /// Logical index of `line` in the `FileDiff.lines` (for diff-cursor focus).
    /// `None` for a filler row opposite a one-sided change.
    idx: Option<usize>,
}

/// Render a [`FileDiff`]: header strip(s), divider, then the body in the current
/// mode. Side-by-side draws old/new panes; unified a single signed column.
fn render_diff(
    frame: &mut Frame,
    dl: &DiffLayout,
    d: &FileDiff,
    view: &ViewState,
    read_only: bool,
    inspect: Option<&str>,
) {
    // Editing mode: the new side carries a live caret at the buffer cursor; the
    // Stage-C logical cursor (hunk revert) is disabled while editing. The live buffer
    // is syntax-highlighted by the file's language (derived from its path). A read-only
    // inspect overlay forces NO caret (the editor belongs to the masked <current> file).
    let lang = lang_of(&d.path);
    let edit = (!read_only)
        .then(|| view.editor.as_ref().filter(|e| e.loaded))
        .flatten()
        .map(|e| EditCtx {
            cursor: (e.cursor_row, e.cursor_col),
            sel: e.selection(),
            lang: &lang,
        });
    let focus_idx = if edit.is_some() {
        None
    } else {
        // A fold-marker row is not a real line - never give it the focus band (the cursor
        // can only sit on one at the initial position; Move skips them).
        (view.focus == Pane::Diff)
            .then_some(view.diff_cursor)
            .filter(|&c| d.lines.get(c).is_none_or(|l| l.fold.is_none()))
    };
    // A long line (word-wrap off) gets a draggable horizontal scrollbar on the body's
    // bottom row; the code rows render above it. `None` when nothing overflows. A read-only
    // overlay uses the browse (no-editor) geometry so its scrollbar matches what it paints.
    let bar = diff_hbar(dl, d, view, if read_only { None } else { view.editor.as_ref() });
    // A file with no changes (decided at open) renders as ONE full-width pane - the same text
    // on both sides would be pure noise. It owns its own (full-width) header. A read-only inspect
    // compare always carries real changes (an identical compare is delivered as a Source), so it
    // never takes the full-width branch even if the underlying preview's flag was set.
    if view.diff_full_width && !read_only {
        render_full_width(frame, dl, d, view, focus_idx, edit, &bar);
        if let Some(b) = &bar {
            render_diff_hbar(frame, b);
        }
        return;
    }
    // A read-only overlay (Compare-with-Revision) carries a descriptive title naming both
    // revs in side order; render it in the header so the compared state is VISIBLE. A normal
    // (non-overlay) diff keeps the rev + path label.
    match inspect {
        Some(title) => render_header(frame, dl.header_old, title, None),
        None => render_header(frame, dl.header_old, &d.old_rev, Some(&d.path)),
    }
    let body_old = body_above_hbar(dl.body_old, &bar);
    match (dl.header_new, dl.body_new, dl.divider) {
        (Some(hn), Some(body_new), Some(div)) => {
            let new_rev = editing_header(view, edit.is_some(), &d.new_rev);
            render_header(frame, hn, &new_rev, None);
            render_divider(frame, div);
            render_side_by_side(frame, body_old, body_above_hbar(body_new, &bar), d, view, focus_idx, edit, dl.blame);
        }
        _ => render_unified(frame, body_old, d, view, focus_idx, edit, dl.blame),
    }
    if let Some(b) = &bar {
        render_diff_hbar(frame, b);
    }
}

/// The rect spanning the WHOLE diff region's body (both panes + divider), for the
/// full-width single-pane render of a no-change file.
fn full_width_body(dl: &DiffLayout) -> Rect {
    Rect { width: full_width(dl), ..dl.body_old }
}

/// The full diff-region width (left header + divider + right body), or just the left
/// width in unified mode (no right pane).
fn full_width(dl: &DiffLayout) -> u16 {
    dl.header_old.width
        + dl.divider.map(|d| d.width).unwrap_or(0)
        + dl.body_new.map(|b| b.width).unwrap_or(0)
}

/// Render a no-change file as ONE full-width pane: the editable buffer (when `edit`) or
/// the committed text (read-only), with a single line-number gutter spanning the whole
/// region. Reuses the new-side row machinery (caret/selection/hscroll), just over a
/// full-width body instead of the right half. The header reads the editing label / the
/// new revision; the left "old" header is not drawn (there is nothing to compare).
fn render_full_width(
    frame: &mut Frame,
    dl: &DiffLayout,
    d: &FileDiff,
    view: &ViewState,
    focus_idx: Option<usize>,
    edit: Option<EditCtx<'_>>,
    bar: &Option<DiffHBar>,
) {
    let header = Rect { width: full_width(dl), ..dl.header_old };
    let label = if edit.is_some() {
        editing_header(view, true, &d.new_rev)
    } else {
        d.new_rev.clone()
    };
    render_header(frame, header, &label, Some(&d.path));
    let body = body_above_hbar(full_width_body(dl), bar);
    if body.height == 0 || body.width == 0 {
        return;
    }
    let (_old_rows, new_rows) = pair_rows(d);
    let focus_row = match edit {
        Some(e) => new_rows.iter().position(|r| line_new_no(r) == Some(e.cursor_line_1based())),
        None => focus_idx.and_then(|c| new_rows.iter().position(|r| r.idx == Some(c))),
    };
    if view.word_wrap {
        let lang = edit.map(|e| e.lang);
        let height = |i: usize| {
            let ne = edit.and_then(|e| line_new_no(&new_rows[i]).map(|n| (n - 1, e)));
            row_code_height(&new_rows[i], view, body.width, ne, lang)
        };
        let window = body.height as usize;
        let scroll = match focus_row {
            Some(fr) => with_edit_scroll(view, edit.is_some(), new_rows.len(), wrapped_scroll(fr, window, new_rows.len(), &height)),
            None => view.diff_scroll,
        };
        render_single_wrapped(frame, body, &new_rows, view, scroll, focus_idx, edit);
    } else {
        let scroll = focused_scroll(view, edit.is_some(), focus_row, new_rows.len(), body.height as usize);
        let code_w = body.width.saturating_sub(GUTTER_W) as usize;
        let hoff = nowrap_hscroll(view, d, edit.map(|e| e.cursor.1), code_w);
        render_pane(frame, body, &new_rows, Side::New, view, scroll, focus_idx, edit, hoff);
    }
}

/// Word-wrap single-pane render (the full-width no-change view): paint the new-side rows
/// down `body`, each consuming its wrapped physical height. Mirrors one half of
/// [`render_paired_wrapped`] without the paired-height lockstep (there is no other side).
#[allow(clippy::too_many_arguments)]
fn render_single_wrapped(
    frame: &mut Frame,
    body: Rect,
    new_rows: &[VisRow<'_>],
    view: &ViewState,
    scroll: usize,
    focus_idx: Option<usize>,
    edit: Option<EditCtx<'_>>,
) {
    if body.height == 0 || body.width == 0 {
        return;
    }
    let mut y = body.y;
    let lang = edit.map(|e| e.lang);
    for row in new_rows.iter().skip(scroll) {
        if y >= body.bottom() {
            break;
        }
        let new_edit = edit.and_then(|e| line_new_no(row).map(|n| (n - 1, e)));
        let h = row_code_height(row, view, body.width, new_edit, lang);
        let avail = (body.bottom() - y) as usize;
        let rh = h.clamp(1, avail) as u16;
        let focused = row.idx.is_some() && row.idx == focus_idx;
        render_wrapped_row(frame, sub_rect(body, y, rh), row, Side::New, view, focused, new_edit, lang);
        y += rh;
    }
}

/// The new-side header label while editing: "current changes (editing)" plus a trailing
/// "*" when the buffer has UNSAVED edits, so a dirty buffer is always visible (the key
/// safeguard when autosave is off and navigating away would drop the edits). The right
/// side IS the live working file; "current changes" matches the `<current>` row + the
/// files-pane "vs" header. Falls back to the diff's own new-revision label when not
/// editing (a read-only historical commit shows its own short hash there).
fn editing_header(view: &ViewState, editing: bool, new_rev: &str) -> String {
    if !editing {
        return new_rev.to_string();
    }
    if view.editor.as_ref().is_some_and(|e| e.dirty) {
        "current changes (editing) *".to_string()
    } else {
        "current changes (editing)".to_string()
    }
}

/// Live editing context for the new (right) side: the buffer cursor `(row, col)`, the
/// optional ordered selection span (both in 0-based buffer coords), and the language
/// to syntax-highlight the live buffer with. Drives the caret cell, the selection
/// highlight, the per-line syntax colors, and the scroll-follow.
#[derive(Clone, Copy)]
struct EditCtx<'a> {
    cursor: (usize, usize),
    sel: Option<((usize, usize), (usize, usize))>,
    lang: &'a str,
}

impl EditCtx<'_> {
    /// The 1-based new-side line the cursor sits on (for scroll-follow + row match).
    fn cursor_line_1based(&self) -> usize {
        self.cursor.0 + 1
    }
    /// Whether buffer position `(row, col)` lies in the selection.
    fn in_sel(&self, row: usize, col: usize) -> bool {
        matches!(self.sel, Some((s, e)) if (row, col) >= s && (row, col) < e)
    }
}

/// Side-by-side: build the paired old/new row columns then paint each pane,
/// numbering from the diff line's own old/new number. When the diff pane is focused
/// the scroll follows the cursor (so the focused line stays on screen).
#[allow(clippy::too_many_arguments)]
fn render_side_by_side(
    frame: &mut Frame,
    body_old: Rect,
    body_new: Rect,
    d: &FileDiff,
    view: &ViewState,
    focus_idx: Option<usize>,
    edit: Option<EditCtx<'_>>,
    blame: Option<Rect>,
) {
    let (old_rows, new_rows) = pair_rows(d);
    let body_h = body_old.height as usize;
    // The visual row to keep on screen: the edit caret's new-side line while editing,
    // else the focused logical line (it sits on whichever side carries it).
    let focus_row = match edit {
        Some(e) => new_rows.iter().position(|r| line_new_no(r) == Some(e.cursor_line_1based())),
        None => focus_idx.and_then(|c| {
            old_rows
                .iter()
                .position(|r| r.idx == Some(c))
                .or_else(|| new_rows.iter().position(|r| r.idx == Some(c)))
        }),
    };
    // Both panes get the edit context: the new side carries the caret/selection, while
    // the old side uses only its `lang` to syntax-highlight the committed (left) text.
    if view.word_wrap {
        // Wrapped: drive BOTH panes from one loop so a logical row occupies the SAME
        // physical rows on each side (pair height = the taller side's wrap), keeping
        // the dual gutters aligned. The editable caret/selection wrap with the text.
        // The scroll must be PHYSICAL-aware (a focused row that wraps must still fit),
        // so derive it from the same per-pair heights the loop paints.
        let lang = edit.map(|e| e.lang);
        let height = |i: usize| {
            let ne = edit.and_then(|e| line_new_no(&new_rows[i]).map(|n| (n - 1, e)));
            row_code_height(&old_rows[i], view, body_old.width, None, lang)
                .max(row_code_height(&new_rows[i], view, body_new.width, ne, lang))
        };
        let window = body_h;
        let scroll = match focus_row {
            Some(fr) => with_edit_scroll(
                view,
                edit.is_some(),
                old_rows.len(),
                wrapped_scroll(fr, window, old_rows.len(), &height),
            ),
            None => view.diff_scroll,
        };
        render_paired_wrapped(frame, body_old, body_new, &old_rows, &new_rows, view, scroll, focus_idx, edit);
    } else {
        let scroll = focused_scroll(view, edit.is_some(), focus_row, old_rows.len(), body_h);
        // One horizontal offset for BOTH panes (computed off the editable new pane's
        // caret) so the left + right stay column-aligned as they scroll sideways.
        let code_w = body_new.width.saturating_sub(GUTTER_W) as usize;
        let hoff = nowrap_hscroll(view, d, edit.map(|e| e.cursor.1), code_w);
        render_pane(frame, body_old, &old_rows, Side::Old, view, scroll, focus_idx, edit, hoff);
        render_pane(frame, body_new, &new_rows, Side::New, view, scroll, focus_idx, edit, hoff);
        // The blame strip aligns with the NEW side's rows at the SAME scroll (no-wrap only).
        if let Some(strip) = blame {
            render_blame_gutter(frame, strip, scroll, new_rows.len(), view, |i| line_new_no(&new_rows[i]));
        }
    }
}

/// Word-wrap side-by-side: paint both panes in lockstep so each logical row spans the
/// SAME physical rows on the left and right (its height is the taller side's wrapped
/// line count), keeping the dual gutters and change bands aligned. The editable new
/// side reuses the per-char spans, so `Paragraph`'s wrap carries the caret + selection
/// to the right wrapped position for free.
#[allow(clippy::too_many_arguments)]
fn render_paired_wrapped(
    frame: &mut Frame,
    body_old: Rect,
    body_new: Rect,
    old_rows: &[VisRow<'_>],
    new_rows: &[VisRow<'_>],
    view: &ViewState,
    scroll: usize,
    focus_idx: Option<usize>,
    edit: Option<EditCtx<'_>>,
) {
    if body_old.height == 0 || body_old.width == 0 {
        return;
    }
    let mut y = body_old.y;
    let lang = edit.map(|e| e.lang);
    for i in scroll..old_rows.len() {
        if y >= body_old.bottom() {
            break;
        }
        let old_row = &old_rows[i];
        let new_row = &new_rows[i];
        let new_edit = edit.and_then(|e| line_new_no(new_row).map(|n| (n - 1, e)));
        let old_h = row_code_height(old_row, view, body_old.width, None, lang);
        let new_h = row_code_height(new_row, view, body_new.width, new_edit, lang);
        let avail = (body_old.bottom() - y) as usize;
        let pair_h = old_h.max(new_h).clamp(1, avail) as u16;
        let old_focused = old_row.idx.is_some() && old_row.idx == focus_idx;
        let new_focused = new_row.idx.is_some() && new_row.idx == focus_idx;
        render_wrapped_row(frame, sub_rect(body_old, y, pair_h), old_row, Side::Old, view, old_focused, None, lang);
        render_wrapped_row(frame, sub_rect(body_new, y, pair_h), new_row, Side::New, view, new_focused, new_edit, lang);
        y += pair_h;
    }
}

/// A `height`-tall sub-rect at `y` within `area`'s horizontal span.
fn sub_rect(area: Rect, y: u16, height: u16) -> Rect {
    Rect { x: area.x, y, width: area.width, height }
}

/// Physical rows a side-by-side row's CODE occupies when word-wrapped into a pane of
/// `pane_w` columns (1 for a filler/empty row). Mirrors what [`render_wrapped_row`]
/// paints so the pair height and the next row's `y` stay in step.
fn row_code_height(
    row: &VisRow<'_>,
    view: &ViewState,
    pane_w: u16,
    edit: Option<(usize, EditCtx<'_>)>,
    hl: Option<&str>,
) -> usize {
    let Some(line) = row.line else { return 1 };
    let code_w = pane_w.saturating_sub(GUTTER_W);
    if code_w == 0 {
        return 1;
    }
    let code = match edit {
        Some((bl, ctx)) => edit_line_spans(line, bl, &ctx, Theme::CODE_BG, view.show_whitespace),
        None => code_spans(line, view, hl),
    };
    wrapped_rows(&code, code_w).max(1) as usize
}

/// Wrapped physical height of one UNIFIED diff line's code within `pane_w` columns
/// minus `prefix_w` (the dual gutters + sign). Mirrors `render_unified_row` so the
/// click walk's row heights match what was painted. 1 for an empty/zero-width line.
fn row_code_height_line(
    line: &DiffLine,
    view: &ViewState,
    pane_w: u16,
    prefix_w: u16,
    edit: Option<(usize, EditCtx<'_>)>,
    hl: Option<&str>,
) -> usize {
    let code_w = pane_w.saturating_sub(prefix_w);
    if code_w == 0 {
        return 1;
    }
    let code = match edit {
        Some((bl, ctx)) => edit_line_spans(line, bl, &ctx, Theme::CODE_BG, view.show_whitespace),
        None => code_spans(line, view, hl),
    };
    wrapped_rows(&code, code_w).max(1) as usize
}

/// Paint one side-by-side row into a `pair_h`-tall rect: fill the whole band, draw the
/// gutter on the first physical row, then the word-wrapped code below. The shorter side
/// of a pair leaves blank band rows so the two panes stay aligned.
#[allow(clippy::too_many_arguments)]
fn render_wrapped_row(
    frame: &mut Frame,
    area: Rect,
    row: &VisRow<'_>,
    side: Side,
    view: &ViewState,
    focused: bool,
    edit: Option<(usize, EditCtx<'_>)>,
    hl: Option<&str>,
) {
    let on_cursor_row = edit.is_some_and(|(bl, e)| bl == e.cursor.0);
    // While a read-only text selection is active the diff-cursor's full-row focus band
    // is suppressed (and the revert gutter hidden), so the per-character selection band
    // is the only highlight - two competing full-row bands read as a broken selection.
    let sel_active = edit.is_none() && diff_sel_active(view);
    let bg = if (focused || on_cursor_row) && !sel_active {
        Theme::SELECTION_FOCUS
    } else {
        row.band.unwrap_or(Theme::CODE_BG)
    };
    fill_row(frame, area, bg);
    let Some(line) = row.line else { return };
    let no = match side {
        Side::Old => line.old_no,
        Side::New => line.new_no,
    };
    let changed = line.kind != LineKind::Context;
    let prefix = if focused && changed && edit.is_none() && !sel_active {
        revert_gutter_span()
    } else {
        gutter_span(no, row.band.is_some())
    };
    let sel = edit
        .is_none()
        .then(|| row.idx.and_then(|i| diff_sel_range(view, i, line_char_len(line))))
        .flatten();
    let code = match edit {
        Some((bl, ctx)) => edit_line_spans(line, bl, &ctx, bg, view.show_whitespace),
        None => match sel {
            Some(r) => readonly_sel_spans(line, hl, view.show_whitespace, bg, r),
            None => code_spans(line, view, hl),
        },
    };
    // Wrapped rows never scroll sideways (the text wraps instead), so `hoff` is 0.
    render_code_row(frame, area, vec![prefix], code, bg, view, 0);
}

/// The new-side line number a visual row carries, if any.
fn line_new_no(row: &VisRow<'_>) -> Option<usize> {
    row.line.and_then(|l| l.new_no)
}

/// The body scroll offset: when a `focus_row` is set (the diff pane is focused) it is
/// derived from the cursor so the focused row stays visible; otherwise the user's
/// `diff_scroll` is used. While `editing`, a wheel free-scroll override
/// (`view.edit_scroll`) wins over the cursor-follow offset.
fn focused_scroll(
    view: &ViewState,
    editing: bool,
    focus_row: Option<usize>,
    total: usize,
    body_h: usize,
) -> usize {
    match focus_row {
        Some(r) => {
            let window = body_h;
            with_edit_scroll(view, editing, total, list_offset(r, total, window))
        }
        None => view.diff_scroll,
    }
}

/// Which line-number a pane reads off a [`DiffLine`].
#[derive(Clone, Copy)]
enum Side {
    Old,
    New,
}

/// Align the diff lines onto two columns. Context lines sit on both sides. A changed
/// block is a maximal Removed run followed by a maximal Added run; the two runs are
/// ZIPPED row-for-row so removed[k] sits opposite added[k] - a multi-line modified
/// block reads as aligned modified rows, NOT a removed-stack ABOVE an added-stack with
/// a screen of empty filler between (which, once "Hide unchanged" folds the surrounding
/// context, fills the viewport with blank rows and reads as "scroll does nothing"). The
/// longer run's tail gets a filler opposite (a pure deletion -> filler on the right, a
/// pure insertion -> filler on the left). This is the only place the side-by-side split
/// lives.
fn pair_rows(d: &FileDiff) -> (Vec<VisRow<'_>>, Vec<VisRow<'_>>) {
    let mut old_rows = Vec::new();
    let mut new_rows = Vec::new();
    let lines = &d.lines;
    let mut i = 0;
    while i < lines.len() {
        match lines[i].kind {
            LineKind::Context => {
                old_rows.push(VisRow { line: Some(&lines[i]), band: None, idx: Some(i) });
                new_rows.push(VisRow { line: Some(&lines[i]), band: None, idx: Some(i) });
                i += 1;
            }
            // A changed block: gather the Removed run, then the Added run, and zip them.
            LineKind::Removed | LineKind::Added => {
                let rm_start = i;
                while i < lines.len() && lines[i].kind == LineKind::Removed {
                    i += 1;
                }
                let (rm_end, add_start) = (i, i);
                while i < lines.len() && lines[i].kind == LineKind::Added {
                    i += 1;
                }
                let (rm_len, add_len) = (rm_end - rm_start, i - add_start);
                for k in 0..rm_len.max(add_len) {
                    // Removed on the left (else a filler opposite a surplus added line).
                    old_rows.push(if k < rm_len {
                        VisRow { line: Some(&lines[rm_start + k]), band: Some(Theme::DIFF_DEL_BG), idx: Some(rm_start + k) }
                    } else {
                        VisRow { line: None, band: Some(Theme::DIFF_ADD_BG), idx: None }
                    });
                    // Added on the right (else a filler opposite a surplus removed line).
                    new_rows.push(if k < add_len {
                        VisRow { line: Some(&lines[add_start + k]), band: Some(Theme::DIFF_ADD_BG), idx: Some(add_start + k) }
                    } else {
                        VisRow { line: None, band: Some(Theme::DIFF_DEL_BG), idx: None }
                    });
                }
            }
        }
    }
    (old_rows, new_rows)
}

/// Paint one side-by-side pane: the visible window of rows (after `diff_scroll`),
/// each as `[gutter | code]` over its band.
#[allow(clippy::too_many_arguments)]
fn render_pane(
    frame: &mut Frame,
    area: Rect,
    rows: &[VisRow<'_>],
    side: Side,
    view: &ViewState,
    scroll: usize,
    focus_idx: Option<usize>,
    edit: Option<EditCtx<'_>>,
    hoff: usize,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    for (y, row) in (area.y..area.bottom()).zip(rows.iter().skip(scroll)) {
        let focused = row.idx.is_some() && row.idx == focus_idx;
        // Editing applies on the NEW side: every line with a `new_no` is an editable
        // buffer line (carries caret + selection), keyed by its 0-based buffer index.
        let edit_line = match edit {
            Some(e) if matches!(side, Side::New) => line_new_no(row).map(|n| (n - 1, e)),
            _ => None,
        };
        render_diff_row(frame, row_rect(area, y), row, side, view, focused, edit_line, edit.map(|e| e.lang), hoff);
    }
}

/// Paint one diff row: fill the band, draw the line-number gutter, then the
/// tokens (with an inline-highlight band on a modified span when present). The gutter
/// stays fixed while the code scrolls sideways by `hoff` columns (word-wrap off).
#[allow(clippy::too_many_arguments)]
fn render_diff_row(
    frame: &mut Frame,
    rect: Rect,
    row: &VisRow<'_>,
    side: Side,
    view: &ViewState,
    focused: bool,
    edit: Option<(usize, EditCtx<'_>)>,
    hl: Option<&str>,
    hoff: usize,
) {
    // The cursor's row (editing) and the focused row (browsing) paint the cursor band;
    // other editable lines keep their change band (selected chars are highlighted
    // per-cell below).
    let on_cursor_row = edit.is_some_and(|(bl, e)| bl == e.cursor.0);
    // A read-only text selection bands its chars per-cell below; while it is active the
    // diff-cursor's full-row focus band (and the revert gutter) is suppressed so it does
    // not read as a second, broken selection.
    let sel_active = edit.is_none() && diff_sel_active(view);
    let bg = if (focused || on_cursor_row) && !sel_active {
        Theme::SELECTION_FOCUS
    } else {
        row.band.unwrap_or(Theme::CODE_BG)
    };
    fill_row(frame, rect, bg);
    let Some(line) = row.line else { return };

    let no = match side {
        Side::Old => line.old_no,
        Side::New => line.new_no,
    };
    let changed = line.kind != LineKind::Context;
    // The focused changed line (browsing) shows the revert icon in its gutter; an
    // editing line keeps the plain number gutter.
    let gutter = if focused && changed && edit.is_none() && !sel_active {
        revert_gutter_span()
    } else {
        gutter_span(no, row.band.is_some())
    };
    let sel = edit
        .is_none()
        .then(|| row.idx.and_then(|i| diff_sel_range(view, i, line_char_len(line))))
        .flatten();
    let code = match edit {
        Some((bl, ctx)) => edit_line_spans(line, bl, &ctx, bg, view.show_whitespace),
        None => match sel {
            Some(r) => readonly_sel_spans(line, hl, view.show_whitespace, bg, r),
            None => code_spans(line, view, hl),
        },
    };
    // The gutter is fixed-width and never scrolls; the code is a separate paragraph in
    // the sub-rect after it, scrolled left by `hoff` so a long line's tail is reachable.
    // Side-by-side rows are always 1 high so the dual-numbered panes stay aligned.
    let gw = GUTTER_W.min(rect.width);
    frame.render_widget(
        Paragraph::new(Line::from(gutter)).style(Style::default().bg(bg)),
        Rect { width: gw, height: 1, ..rect },
    );
    let code_x = rect.x + GUTTER_W;
    if code_x < rect.right() {
        let code_area = Rect { x: code_x, y: rect.y, width: rect.right() - code_x, height: 1 };
        let mut para = Paragraph::new(Line::from(code)).style(Style::default().bg(bg));
        if hoff > 0 {
            para = para.scroll((0, hoff as u16));
        }
        frame.render_widget(para, code_area);
    }
}

/// Unified: one column of `[old-no | new-no | sign | code]`, removed lines on a
/// red band, added on green, context plain.
#[allow(clippy::too_many_arguments)]
fn render_unified(
    frame: &mut Frame,
    area: Rect,
    d: &FileDiff,
    view: &ViewState,
    focus_idx: Option<usize>,
    edit: Option<EditCtx<'_>>,
    blame: Option<Rect>,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let window = area.height as usize;
    let prefix_w = GUTTER_W * 2 + 2;
    let lang = edit.map(|e| e.lang);
    // The row to keep on screen: the caret's line while editing, else the focused row.
    let focus = match edit {
        Some(e) => d.lines.iter().position(|l| l.new_no == Some(e.cursor_line_1based())),
        None => focus_idx,
    };
    // Editing/focused: scroll to keep that row visible (PHYSICAL-aware under wrap, so a
    // wrapped row above the caret cannot push it off the bottom); else free scroll.
    let scroll = match focus {
        Some(f) if view.word_wrap => {
            let height = |i: usize| {
                let ei = edit.and_then(|e| d.lines[i].new_no.map(|n| (n - 1, e)));
                row_code_height_line(&d.lines[i], view, area.width, prefix_w, ei, lang)
            };
            with_edit_scroll(
                view,
                edit.is_some(),
                d.lines.len(),
                wrapped_scroll(f, window, d.lines.len(), &height),
            )
        }
        Some(f) => with_edit_scroll(view, edit.is_some(), d.lines.len(), list_offset(f, d.lines.len(), window)),
        None => view.diff_scroll,
    };
    // Horizontal offset (word-wrap off): the wheel override, else cursor-follow when
    // editing. Applied to the code column only; the dual gutters + sign stay fixed.
    let code_w = area.width.saturating_sub(prefix_w) as usize;
    let hoff = nowrap_hscroll(view, d, edit.map(|e| e.cursor.1), code_w);
    let mut y = area.y;
    for (i, line) in d.lines.iter().enumerate().skip(scroll) {
        if y >= area.bottom() {
            break;
        }
        let focused = focus_idx == Some(i);
        let sel_active = edit.is_none() && diff_sel_active(view);
        let sel = edit
            .is_none()
            .then(|| diff_sel_range(view, i, line_char_len(line)))
            .flatten();
        let edit_line = edit.and_then(|e| line.new_no.map(|n| (n - 1, e)));
        y += render_unified_row(frame, span_rect(area, y), line, view, focused, sel, sel_active, edit_line, edit.map(|e| e.lang), hoff);
    }
    // Blame strip keyed by each unified line's NEW number, at the same scroll (no-wrap only).
    if let Some(strip) = blame {
        render_blame_gutter(frame, strip, scroll, d.lines.len(), view, |i| d.lines[i].new_no);
    }
}

/// One unified row: dual gutters, a +/-/space sign, then the tokens over the band.
/// Returns the physical rows consumed (>1 when word-wrap splits the code). A focused
/// row shows the revert icon (browsing); an editing line shows the caret + selection.
#[allow(clippy::too_many_arguments)]
fn render_unified_row(
    frame: &mut Frame,
    area: Rect,
    line: &DiffLine,
    view: &ViewState,
    focused: bool,
    sel: Option<(usize, usize)>,
    sel_active: bool,
    edit: Option<(usize, EditCtx<'_>)>,
    hl: Option<&str>,
    hoff: usize,
) -> u16 {
    let (band_bg, sign, changed) = match line.kind {
        LineKind::Context => (Theme::CODE_BG, ' ', false),
        LineKind::Added => (Theme::DIFF_ADD_BG, '+', true),
        LineKind::Removed => (Theme::DIFF_DEL_BG, '-', true),
    };
    let on_cursor_row = edit.is_some_and(|(bl, e)| bl == e.cursor.0);
    // An active read-only selection bands its chars per-cell; suppress the full-row
    // focus band (and the revert gutter) while it is, so the selection reads cleanly.
    let bg = if (focused || on_cursor_row) && !sel_active {
        Theme::SELECTION_FOCUS
    } else {
        band_bg
    };
    let old_gutter = if focused && changed && edit.is_none() && !sel_active {
        revert_gutter_span()
    } else {
        gutter_span(line.old_no, changed)
    };
    let prefix = vec![old_gutter, gutter_span(line.new_no, changed), sign_span(sign, line.kind)];
    let code = match edit {
        Some((bl, ctx)) => edit_line_spans(line, bl, &ctx, bg, view.show_whitespace),
        None => match sel {
            Some(r) => readonly_sel_spans(line, hl, view.show_whitespace, bg, r),
            None => code_spans(line, view, hl),
        },
    };
    render_code_row(frame, area, prefix, code, bg, view, hoff)
}

/// Code spans for an editing buffer line `bl`: the text rendered char-by-char,
/// syntax-colored by `ctx.lang`, with the caret cell reversed and selected chars on
/// the selection band (which keeps each char's syntax color so highlighting survives
/// a selection). `row_bg` is the line's base background. Chars pass through
/// [`render_char`] so a tab becomes a single space/marker (`show_ws`) - keeping ONE
/// cell per char, which the click-to-place + caret math rely on, and honoring the
/// Whitespace toggle on the editable side too. The per-line highlight is cheap
/// because only the visible rows are ever rendered.
fn edit_line_spans(line: &DiffLine, bl: usize, ctx: &EditCtx<'_>, row_bg: Color, show_ws: bool) -> Vec<Span<'static>> {
    let text: String = line.tokens.iter().map(|t| t.text.as_str()).collect();
    let chars: Vec<char> = text.chars().collect();
    // One syntax color per char, expanded from the highlighted token runs.
    let mut fg: Vec<Color> = Vec::with_capacity(chars.len());
    for tok in highlight_line(ctx.lang, &text) {
        let color = tok.kind.color();
        fg.extend(std::iter::repeat_n(color, tok.text.chars().count()));
    }
    let caret = Style::default().bg(Theme::TEXT).fg(Theme::FIELD_BG);
    let is_caret = |col: usize| (bl, col) == ctx.cursor;
    let mut spans = Vec::with_capacity(chars.len() + 1);
    for (col, ch) in chars.iter().enumerate() {
        let base = fg.get(col).copied().unwrap_or(Theme::TEXT);
        let color = marker_fg(base, is_ws_marker(*ch, show_ws));
        let style = if is_caret(col) {
            caret
        } else if ctx.in_sel(bl, col) {
            Style::default().bg(Theme::SELECTION_EDIT).fg(color)
        } else {
            Style::default().bg(row_bg).fg(color)
        };
        spans.push(Span::styled(render_char(*ch, show_ws).to_string(), style));
    }
    // Caret at or past end-of-line: a trailing cursor block.
    if ctx.cursor.0 == bl && ctx.cursor.1 >= chars.len() {
        spans.push(Span::styled(" ".to_string(), caret));
    }
    spans
}

// -- source preview --------------------------------------------------------

/// Render an unchanged file as a single-pane, line-numbered, syntax-highlighted
/// preview with no change bands.
fn render_source(frame: &mut Frame, dl: &DiffLayout, s: &SourceFile, view: &ViewState, inspect: Option<&str>) {
    // A plain (unchanged) file has no "after" side to diff against, so render it FULL
    // WIDTH across both panes (header + body) instead of leaving the right pane blank.
    // No divider is drawn for a source; the gap is just the dark code canvas from `render`.
    // An inspect overlay labels its header with the revision (e.g. "HEAD - Esc to close")
    // instead of the language, so the read-only revision view is clearly distinguished.
    let total_w = full_width(dl);
    let label = inspect.unwrap_or(&s.lang);
    render_header(frame, Rect { width: total_w, ..dl.header_old }, label, Some(&s.path));
    let area = Rect { width: total_w, ..dl.body_old };
    if area.height == 0 || area.width == 0 {
        return;
    }
    let mut y = area.y;
    for (idx, tokens) in s.lines.iter().enumerate().skip(view.diff_scroll) {
        if y >= area.bottom() {
            break;
        }
        let prefix = vec![gutter_span(Some(idx + 1), false)];
        let code = token_spans(tokens, None, view.show_whitespace);
        // The plain source view has no caret + no horizontal wheel routing, so `hoff` 0.
        y += render_code_row(frame, span_rect(area, y), prefix, code, Theme::CODE_BG, view, 0);
    }
}

/// Fixed widths for the blame gutter columns (left of the line-number gutter): an
/// 8-char short hash, the abbreviated author, and the compact date. Tabular, so they
/// pad/truncate to a constant width and the code column starts at the same place every row.
const BLAME_HASH_W: usize = 8;
const BLAME_AUTHOR_W: usize = 14;
const BLAME_DATE_W: usize = 10;

/// Render a [`BlameFile`]: a read-only single-pane source view (like [`render_source`]) with
/// a per-line blame gutter - `hash author date` - prepended to the line-number gutter. An
/// uncommitted working-tree line shows a blank hash + the "Not Committed Yet" author git
/// reports. Vertical scroll follows `view.diff_scroll`; no caret, no horizontal scroll.
fn render_blame(frame: &mut Frame, dl: &DiffLayout, b: &BlameFile, view: &ViewState, inspect: Option<&str>) {
    let total_w = full_width(dl);
    render_header(frame, Rect { width: total_w, ..dl.header_old }, inspect.unwrap_or("blame"), Some(&b.path));
    let area = Rect { width: total_w, ..dl.body_old };
    if area.height == 0 || area.width == 0 {
        return;
    }
    let mut y = area.y;
    for (idx, line) in b.lines.iter().enumerate().skip(view.diff_scroll) {
        if y >= area.bottom() {
            break;
        }
        let mut prefix = blame_gutter_spans(line);
        prefix.push(gutter_span(Some(idx + 1), false));
        let code = token_spans(&line.tokens, None, view.show_whitespace);
        y += render_code_row(frame, span_rect(area, y), prefix, code, Theme::CODE_BG, view, 0);
    }
}

/// The blame gutter for one line: short hash (dim), abbreviated author (dim), date (faint),
/// each padded to a constant width so the line-number gutter + code align across rows. An
/// uncommitted working line (blank hash) collapses the hash+author columns into one faint
/// "uncommitted" label - clearer than git's "Not Committed Yet" run through the abbreviator.
fn blame_gutter_spans(line: &BlameLine) -> Vec<Span<'static>> {
    let date = blame_field(&line.date, BLAME_DATE_W);
    let mut spans = if line.commit.is_empty() {
        vec![Span::styled(
            format!("{} ", blame_field("uncommitted", BLAME_HASH_W + 1 + BLAME_AUTHOR_W)),
            Style::default().fg(Theme::TEXT_FAINT),
        )]
    } else {
        let hash = blame_field(&line.commit, BLAME_HASH_W);
        let author = blame_field(&abbrev_author(&line.author), BLAME_AUTHOR_W);
        vec![
            Span::styled(format!("{hash} "), Style::default().fg(Theme::TEXT_DIM)),
            Span::styled(format!("{author} "), Style::default().fg(Theme::TEXT_DIM)),
        ]
    };
    spans.push(Span::styled(format!("{date} "), Style::default().fg(Theme::TEXT_FAINT)));
    spans
}

/// Truncate `s` to `w` display columns, then right-pad with spaces to exactly `w` - a
/// fixed-width tabular cell for the blame gutter.
fn blame_field(s: &str, w: usize) -> String {
    let t = truncate(s, w);
    format!("{t}{}", " ".repeat(w.saturating_sub(str_width(&t))))
}

/// Render the View > Blame gutter STRIP (carved off the editable side's left in the layout):
/// one compact `hash author` line per VISIBLE new-side row, keyed by the row's NEW line number,
/// at the SAME vertical `scroll` as the diff body so the gutter stays lockstep with the code.
/// A row with no new-side line (a deletion) or past the blame length renders blank.
fn render_blame_gutter(
    frame: &mut Frame,
    strip: Rect,
    scroll: usize,
    total: usize,
    view: &ViewState,
    new_no_at: impl Fn(usize) -> Option<usize>,
) {
    let Some(blame) = view.blame.as_ref().filter(|b| !b.lines.is_empty()) else {
        return;
    };
    let w = strip.width as usize;
    for y in 0..strip.height {
        let idx = scroll + y as usize;
        let text = (idx < total)
            .then(|| new_no_at(idx))
            .flatten()
            .and_then(|n| blame.lines.get(n - 1))
            .map(|bl| compact_blame(bl, w))
            .unwrap_or_else(|| " ".repeat(w));
        let cell = Rect { y: strip.y + y, height: 1, ..strip };
        frame.render_widget(
            Paragraph::new(Span::styled(text, Style::default().bg(Theme::CODE_BG).fg(Theme::TEXT_DIM))),
            cell,
        );
    }
}

/// One blame strip line: `hash author` packed into `w` columns (an uncommitted working line
/// collapses to a faint "uncommitted" label). The short hash leads, the abbreviated author
/// fills the rest - a compact gutter, narrower than the full `render_blame` overlay.
fn compact_blame(line: &BlameLine, w: usize) -> String {
    if line.commit.is_empty() {
        return blame_field("uncommitted", w);
    }
    let hash_w = 7.min(w);
    let hash = blame_field(&line.commit, hash_w);
    let rest = w.saturating_sub(hash_w + 1);
    format!("{hash} {}", blame_field(&abbrev_author(&line.author), rest))
}

/// Binary state: the header (path) plus a centered "Binary file differs" notice
/// over the code canvas, so a binary change reads clearly instead of as a blank
/// diff body. The single non-text preview state.
fn render_binary(frame: &mut Frame, dl: &DiffLayout, b: &BinaryFile) {
    render_header(frame, Rect { width: full_width(dl), ..dl.header_old }, "binary", Some(&b.path));
    centered_notice(frame, dl.body_old, &b.note, Theme::TEXT_DIM, Theme::CODE_BG);
}

/// Empty state: a dim notice when nothing is selected for preview.
fn render_empty(frame: &mut Frame, dl: &DiffLayout) {
    // Full-width header (no two-pane split to label): a left-only header band would leave
    // the right half of the row on the dark canvas, reading as the toolbar color bleeding in.
    render_header(frame, Rect { width: full_width(dl), ..dl.header_old }, "no selection", None);
    let area = dl.body_old;
    if area.height < 2 {
        return;
    }
    // A single-row notice. Keeping `area`'s full height here would paint the CODE_BG band
    // one row PAST the diff body (y+1 with the same height overruns the bottom) into the
    // hsep row below - a left-pane-only seam that reads as the diff background leaking into
    // the toolbar separator. Pin height to 1 so the band never exceeds the body.
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "  select a file to preview",
            Style::default().fg(Theme::TEXT_FAINT),
        )))
        .style(Style::default().bg(Theme::CODE_BG)),
        Rect { y: area.y + 1, height: 1, ..area },
    );
}

/// Loading state: a file is selected but the loader has not delivered its preview
/// yet. A centered notice over the code canvas, filled by the next PreviewLoaded.
fn render_loading(frame: &mut Frame, dl: &DiffLayout) {
    render_header(frame, Rect { width: full_width(dl), ..dl.header_old }, "loading", None);
    centered_notice(frame, dl.body_old, "Loading diff...", Theme::TEXT_DIM, Theme::CODE_BG);
}

// -- shared row pieces -----------------------------------------------------

/// A single-row rect at `y` within `area`'s horizontal span.
fn row_rect(area: Rect, y: u16) -> Rect {
    Rect { x: area.x, y, width: area.width, height: 1 }
}

/// A rect at `y` spanning the remaining body height (so a wrapped row may grow
/// downward to its full line count).
fn span_rect(area: Rect, y: u16) -> Rect {
    Rect {
        x: area.x,
        y,
        width: area.width,
        height: area.bottom().saturating_sub(y),
    }
}

/// Render one logical code row inside `area` (which may be several rows tall):
/// the fixed `prefix` (gutter[s] + optional sign) on the first physical row, then
/// the `code` to its right. With word-wrap off the code is clipped to one row;
/// with it on the code wraps and the row consumes as many physical rows as it
/// needs. The band `bg` fills every consumed row. Returns the rows consumed.
fn render_code_row(
    frame: &mut Frame,
    area: Rect,
    prefix: Vec<Span<'static>>,
    code: Vec<Span<'static>>,
    bg: Color,
    view: &ViewState,
    hoff: usize,
) -> u16 {
    let prefix_w: u16 = prefix.iter().map(|s| s.content.chars().count() as u16).sum();
    let code_w = area.width.saturating_sub(prefix_w);
    let code_area = Rect { x: area.x + prefix_w, y: area.y, width: code_w, ..area };

    // How many physical rows the code occupies (1 when not wrapping).
    let rows = if view.word_wrap && code_w > 0 {
        wrapped_rows(&code, code_w).clamp(1, area.height.max(1))
    } else {
        1
    };

    let band = Rect { height: rows.min(area.height), ..area };
    fill_row(frame, band, bg);
    frame.render_widget(
        Paragraph::new(Line::from(prefix)).style(Style::default().bg(bg)),
        Rect { height: 1, ..area },
    );
    let mut code_para = Paragraph::new(Line::from(code)).style(Style::default().bg(bg));
    if view.word_wrap {
        code_para = code_para.wrap(Wrap { trim: false });
    } else if hoff > 0 {
        // No-wrap: scroll the code left by `hoff` columns so a long line's tail shows.
        code_para = code_para.scroll((0, hoff as u16));
    }
    frame.render_widget(code_para, Rect { height: rows.min(area.height), ..code_area });
    rows.min(area.height).max(1)
}

/// Physical row count when the `code` spans flow into `width`-wide rows, wrapping
/// at word boundaries (matching `Wrap { trim: false }`) and hard-breaking words
/// longer than `width`. Mirrors the wrap the renderer applies so the band height
/// and the next row's `y` stay in step.
fn wrapped_rows(code: &[Span<'static>], width: u16) -> u16 {
    let text: String = code.iter().map(|s| s.content.as_ref()).collect();
    let w = width as usize;
    let mut rows: u16 = 1;
    let mut col = 0usize;
    for word in split_keep_spaces(&text) {
        let len = word.chars().count();
        if col + len > w && col > 0 {
            rows = rows.saturating_add(1);
            col = 0;
        }
        col += len;
        while col > w {
            rows = rows.saturating_add(1);
            col -= w;
        }
    }
    rows
}

/// Split into words while keeping each trailing space attached, so wrap-width
/// accounting matches the renderer (whitespace counts toward the column).
fn split_keep_spaces(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        cur.push(ch);
        if ch == ' ' {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Fill a whole row with `bg` so a change band spans the full pane width even
/// where no token text is written.
fn fill_row(frame: &mut Frame, rect: Rect, bg: Color) {
    frame.render_widget(Block::default().style(Style::default().bg(bg)), rect);
}

/// Right-aligned line-number gutter span. A changed line gets the brighter
/// gutter color; a `None` number renders blank (e.g. the absent side of a
/// one-sided change).
fn gutter_span(no: Option<usize>, changed: bool) -> Span<'static> {
    let text = match no {
        Some(n) => format!("{n:>width$} ", width = (GUTTER_W - 1) as usize),
        None => " ".repeat(GUTTER_W as usize),
    };
    let fg = if changed { Theme::GUTTER_HL } else { Theme::TEXT_FAINT };
    Span::styled(text, Style::default().fg(fg))
}

/// The focused changed line's gutter: the revert hook-arrow icon (U+21A9),
/// right-aligned in the gutter width, in the close-accent color (Enter reverts the
/// hunk). Same width as [`gutter_span`] so the code column never shifts.
fn revert_gutter_span() -> Span<'static> {
    let pad = (GUTTER_W as usize).saturating_sub(2);
    Span::styled(
        format!("{}{} ", " ".repeat(pad), Glyph::REVERT),
        Style::default().fg(Theme::ACCENT_CLOSE).bold(),
    )
}

/// The unified-mode +/-/space sign cell (2 cols: sign + pad), colored by kind.
fn sign_span(sign: char, kind: LineKind) -> Span<'static> {
    let fg = match kind {
        LineKind::Added => Theme::ACCENT_RUN,
        LineKind::Removed => Theme::ACCENT_CLOSE,
        LineKind::Context => Theme::TEXT_FAINT,
    };
    Span::styled(format!("{sign} "), Style::default().fg(fg))
}

/// Code spans for a diff line: tokens colored by kind, with the inline-highlight
/// span (if any) over a stronger band. Full width - the row renderer clips or
/// wraps to the pane. When `hl` is `Some(lang)` (the live editable diff, whose
/// lines are not pre-tokenized) the line is syntax-highlighted on the fly; for a
/// read-only diff the tokens already carry their kinds, so `hl` is `None`.
fn code_spans(line: &DiffLine, view: &ViewState, hl: Option<&str>) -> Vec<Span<'static>> {
    if let Some(hidden) = line.fold {
        return fold_marker_spans(hidden);
    }
    let inline = line.inline_hl.map(|range| (range, inline_band(line.kind)));
    match hl {
        Some(lang) => {
            let text: String = line.tokens.iter().map(|t| t.text.as_str()).collect();
            token_spans(&highlight_line(lang, &text), inline, view.show_whitespace)
        }
        None => token_spans(&line.tokens, inline, view.show_whitespace),
    }
}

/// Stronger inline band color for a modified span, by line kind.
fn inline_band(kind: LineKind) -> Color {
    match kind {
        LineKind::Added => Theme::INLINE_ADD,
        LineKind::Removed => Theme::INLINE_DEL,
        LineKind::Context => Theme::DIFF_CHG_BG,
    }
}

/// Turn tokens into styled spans. When `inline` is set, characters whose offset
/// falls in `[start, end)` get the inline-highlight background. `show_ws`
/// renders tabs as visible markers. Produces the full line; the row renderer
/// clips (no wrap) or wraps to the pane width.
fn token_spans(
    tokens: &[Token],
    inline: Option<((usize, usize), Color)>,
    show_ws: bool,
) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut col = 0usize; // char offset within the logical line
    for tok in tokens {
        let fg = tok.kind.color();
        for span in split_token(tok, &mut col, inline, show_ws, fg) {
            out.push(span);
        }
    }
    out
}

/// Emit the styled spans for one token, advancing the char offset `col`. Splits
/// on the inline-highlight boundary so only the changed characters carry the
/// stronger band.
fn split_token(
    tok: &Token,
    col: &mut usize,
    inline: Option<((usize, usize), Color)>,
    show_ws: bool,
    fg: Color,
) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut buf_hl = false;
    let mut buf_ws = false;
    for ch in tok.text.chars() {
        let hl = char_in_inline(*col, inline);
        let ws = is_ws_marker(ch, show_ws);
        // Flush on either boundary: the inline-highlight run OR the whitespace-marker
        // run (markers read faint), so each span carries one color + one band.
        if !buf.is_empty() && (hl != buf_hl || ws != buf_ws) {
            out.push(make_span(std::mem::take(&mut buf), marker_fg(fg, buf_ws), buf_hl, inline));
        }
        if buf.is_empty() {
            buf_hl = hl;
            buf_ws = ws;
        }
        buf.push(render_char(ch, show_ws));
        *col += 1;
    }
    if !buf.is_empty() {
        out.push(make_span(buf, marker_fg(fg, buf_ws), buf_hl, inline));
    }
    out
}

/// Whether char offset `col` lies in the inline-highlight range.
fn char_in_inline(col: usize, inline: Option<((usize, usize), Color)>) -> bool {
    matches!(inline, Some(((start, end), _)) if col >= start && col < end)
}

/// Build a token span, adding the inline-highlight background (and bold) when
/// the run is highlighted.
fn make_span(
    text: String,
    fg: Color,
    hl: bool,
    inline: Option<((usize, usize), Color)>,
) -> Span<'static> {
    match (hl, inline) {
        (true, Some((_, band))) => {
            Span::styled(text, Style::default().fg(fg).bg(band).bold())
        }
        _ => Span::styled(text, Style::default().fg(fg)),
    }
}

/// Map a source char to its rendered glyph. With `show_ws`, a space becomes a faint
/// mid-dot and a tab a faint arrow (each ONE cell, so the caret/column math is
/// unchanged); without it a tab collapses to a single space and the rest pass through.
fn render_char(ch: char, show_ws: bool) -> char {
    match ch {
        ' ' if show_ws => Glyph::WS_SPACE,
        '\t' if show_ws => Glyph::WS_TAB,
        '\t' => ' ',
        c => c,
    }
}

/// Whether `ch` renders as a whitespace marker (so it should read in the faint color
/// rather than its syntax color). Only true while the Whitespace toggle is on.
fn is_ws_marker(ch: char, show_ws: bool) -> bool {
    show_ws && (ch == ' ' || ch == '\t')
}

/// A whitespace-marker run reads faint; everything else keeps its base color.
fn marker_fg(base: Color, is_marker: bool) -> Color {
    if is_marker {
        Theme::TEXT_FAINT
    } else {
        base
    }
}

// -- header / divider / fold marker ---------------------------------------

/// One header strip cell: the revision/lang (bright bold) and an optional path
/// (dim), preceded by the lock-substitute glyph, clipped to width, over the
/// toolbar band.
fn render_header(frame: &mut Frame, area: Rect, rev: &str, path: Option<&str>) {
    if area.width == 0 {
        return;
    }
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(
            format!("{} ", Glyph::REV_LOCK),
            Style::default().fg(Theme::TEXT_DIM),
        ),
        Span::styled(rev.to_string(), Style::default().fg(Theme::TEXT).bold()),
    ];
    if let Some(p) = path {
        let budget = area.width.saturating_sub(rev.chars().count() as u16 + 6) as usize;
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            truncate(p, budget.max(4)),
            Style::default().fg(Theme::TEXT_DIM),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(Theme::BG_TOOLBAR)),
        area,
    );
}

/// Vertical center divider between the two panes.
fn render_divider(frame: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let lines: Vec<Line> = (0..area.height).map(|_| Line::raw("\u{2502}")).collect();
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().fg(Theme::BORDER).bg(Theme::CODE_BG)),
        area,
    );
}

/// The code-column spans for an interior fold marker (git's omitted -U3 context): a faint
/// zigzag-flanked "N unchanged lines" summary. Rendered in place of a real line's code,
/// after the marker's blank gutter (both line numbers are `None`), in every diff mode.
fn fold_marker_spans(hidden: usize) -> Vec<Span<'static>> {
    let z = Glyph::FOLD;
    let plural = if hidden == 1 { "" } else { "s" };
    vec![Span::styled(
        format!("{z}{z}{z} {hidden} unchanged line{plural} {z}{z}{z}"),
        Style::default().fg(Theme::TEXT_FAINT),
    )]
}

#[cfg(test)]
mod geometry_tests {
    use super::*;

    #[test]
    fn wrapped_scroll_keeps_a_bottom_focus_visible_under_wrap() {
        // 10 rows, each wrapping to 2 physical rows, in a 6-physical-row viewport. A
        // logical-row offset (list_offset) would return 9-? and let the caret fall off;
        // the physical-aware scroll must show rows 7,8,9 (3 rows x 2 = 6 = viewport).
        let s = wrapped_scroll(9, 6, 10, &|_| 2);
        assert_eq!(s, 7, "focus row 9 sits at the viewport bottom (rows 7..=9 fill 6 physical rows)");
    }

    #[test]
    fn wrapped_scroll_shows_an_oversized_focus_row_from_its_top() {
        // The focus row alone is taller than the whole viewport: start AT it (show its
        // top), never above it.
        let s = wrapped_scroll(5, 6, 10, &|i| if i == 5 { 10 } else { 1 });
        assert_eq!(s, 5);
    }

    #[test]
    fn wrapped_scroll_no_scroll_when_everything_fits() {
        assert_eq!(wrapped_scroll(4, 20, 5, &|_| 1), 0, "5 single rows fit in 20 -> top");
    }

    fn hbar(cur: usize) -> DiffHBar {
        // 40-cell track over a 100-col line with 60 hidden (max=60), so the thumb has real
        // travel and a width < track - the regime where a naive col->offset map jumps.
        DiffHBar { track: Rect { x: 10, y: 0, width: 40, height: 1 }, cur, max: 60, content_w: 100 }
    }

    #[test]
    fn hbar_press_on_thumb_does_not_jump() {
        // A press anywhere INSIDE the current thumb keeps the offset (cur) - the re-click
        // jump bug was mapping the click column straight to a (different) offset.
        let bar = hbar(30);
        let (tx, tw) = hbar_thumb(&bar);
        for col in tx..tx + tw {
            let (offset, grab) = hbar_press_at(&bar, col);
            assert_eq!(offset, 30, "press on thumb col {col} must hold the offset");
            assert_eq!(grab, col - tx, "grab is the point within the thumb");
        }
    }

    #[test]
    fn hbar_thumb_drag_tracks_the_grab_point() {
        // Grabbing the thumb then dragging keeps the grabbed cell under the pointer: the
        // thumb's left edge follows (col - grab), mapped back through the travel.
        let bar = hbar(30);
        let (tx, tw) = hbar_thumb(&bar);
        let grab = tw / 2;
        // Drag one cell right of where we grabbed -> offset increases, monotonically.
        let here = hbar_offset_for_thumb_x(&bar, (tx + grab).saturating_sub(grab));
        let right = hbar_offset_for_thumb_x(&bar, (tx + grab + 1).saturating_sub(grab));
        assert!(right > here, "dragging right raises the offset ({here} -> {right})");
        // Dragging to the far right pins at max; far left at 0.
        assert_eq!(hbar_offset_for_thumb_x(&bar, 9999), 60);
        assert_eq!(hbar_offset_for_thumb_x(&bar, 0), 0);
    }

    #[test]
    fn hbar_track_click_outside_thumb_centers_it() {
        // A press on the bare track (not the thumb) centers the thumb under the pointer and
        // grabs its middle, so the very next drag is smooth (no initial snap).
        let bar = hbar(0);
        let (_, tw) = hbar_thumb(&bar);
        let far = bar.track.x + bar.track.width - 2; // near the right end, past the thumb
        let (offset, grab) = hbar_press_at(&bar, far);
        assert_eq!(grab, tw / 2, "the click grabs the thumb's middle");
        assert!(offset > 0, "clicking far right scrolls toward the end (got {offset})");
    }

    #[test]
    fn with_edit_scroll_overrides_only_while_editing_and_clamps() {
        let mut view = ViewState::new(0);
        // No override -> the cursor-follow offset is used, editing or not.
        assert_eq!(with_edit_scroll(&view, true, 10, 4), 4);
        assert_eq!(with_edit_scroll(&view, false, 10, 4), 4);
        // Override set: it wins ONLY while editing; browsing keeps the follow offset so
        // the golden render is untouched.
        view.edit_scroll = Some(7);
        assert_eq!(with_edit_scroll(&view, true, 10, 4), 7, "editing honors the free top");
        assert_eq!(with_edit_scroll(&view, false, 10, 4), 4, "browsing ignores it");
        // A stale override past the last row clamps to n-1 (never blanks the pane).
        view.edit_scroll = Some(99);
        assert_eq!(with_edit_scroll(&view, true, 10, 4), 9);
    }

    #[test]
    fn wrap_row_starts_breaks_at_word_boundaries_not_uniform_width() {
        // "aaa bbbbbbbbbb ccc" in width 10: row 0 starts at 0, the long word is pushed to
        // a new row at char 4 (NOT char 10), then hard-breaks at char 14; "ccc" rejoins.
        let starts = wrap_row_starts("aaa bbbbbbbbbb ccc", 10);
        assert_eq!(starts, vec![0, 4, 14], "row starts follow word wrap, not sub*width");
    }

    #[test]
    fn wrap_row_starts_single_row_when_it_fits() {
        assert_eq!(wrap_row_starts("short line", 40), vec![0]);
    }
}

#[cfg(test)]
mod edit_click_tests {
    use super::*;
    use crate::textdiff::live_diff;
    use crate::view_state::EditorState;

    fn diff(base: &str, work: &str) -> FileDiff {
        live_diff(
            &base.split('\n').map(str::to_string).collect::<Vec<_>>(),
            &work.split('\n').map(str::to_string).collect::<Vec<_>>(),
            "f",
            "wt",
        )
    }

    #[test]
    fn pair_rows_count_matches_visual_rows_for_unequal_modified_block() {
        // b,c,d -> X (3 Removed, 1 Added): the zip makes max(3,1)=3 rows, NOT 4. The
        // side-by-side scroll clamp (visual_rows) MUST equal pair_rows' row count, or the
        // body scrolls past its last real row into blank filler.
        let d = diff("a\nb\nc\nd\ne", "a\nX\ne");
        let (old_rows, new_rows) = pair_rows(&d);
        assert_eq!(old_rows.len(), new_rows.len(), "panes must have equal row counts");
        let clamp = FileView::Diff(d.clone()).visual_rows(true);
        assert_eq!(new_rows.len(), clamp, "pair_rows count must mirror visual_rows");
    }

    /// A side-by-side layout: new pane at x=40, body rows from y=2.
    fn sbs_layout() -> DiffLayout {
        let body_new = Rect { x: 40, y: 2, width: 40, height: 20 };
        DiffLayout {
            header_old: Rect { x: 0, y: 1, width: 39, height: 1 },
            header_new: Some(Rect { x: 40, y: 1, width: 40, height: 1 }),
            body_old: Rect { x: 0, y: 2, width: 39, height: 20 },
            body_new: Some(body_new),
            divider: Some(Rect { x: 39, y: 2, width: 1, height: 20 }),
            blame: None,
        }
    }

    #[test]
    fn editable_code_x_range_spans_the_new_pane_and_is_none_under_word_wrap() {
        let dl = sbs_layout(); // body_new x=40 width=40 -> code starts at 40+GUTTER_W, ends at 79
        let mut view = ViewState::new(0);
        view.word_wrap = false;
        let (left, right) = editable_code_x_range(&dl, &view).expect("a horizontal axis exists");
        assert_eq!(left, 40 + GUTTER_W, "code starts past the new-side gutter");
        assert_eq!(right, 79, "right edge is the last paintable column of the new pane");
        // Word-wrap removes the horizontal axis -> no edge-scroll.
        view.word_wrap = true;
        assert!(editable_code_x_range(&dl, &view).is_none(), "no edge-scroll while wrapping");
    }

    /// A unified layout: a single 80-wide body column from y=2 (no new pane / divider).
    fn unified_layout() -> DiffLayout {
        DiffLayout {
            header_old: Rect { x: 0, y: 1, width: 80, height: 1 },
            header_new: None,
            body_old: Rect { x: 0, y: 2, width: 80, height: 20 },
            body_new: None,
            divider: None,
            blame: None,
        }
    }

    #[test]
    fn locate_diff_click_maps_a_read_only_click_to_its_logical_line_and_column() {
        // An all-context diff (base == work): every line shows on BOTH sides, read-only.
        let src = (0..10).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let d = diff(&src, &src);
        let view = ViewState::new(0); // focus Log, diff_scroll 0

        // Unified: the body starts at y=2, so row 2 -> line 0, row 5 -> line 3.
        let ul = unified_layout();
        assert_eq!(locate_diff_click(&ul, &d, &view, 5, 2).map(|(l, _)| l), Some(0));
        assert_eq!(locate_diff_click(&ul, &d, &view, 5, 5).map(|(l, _)| l), Some(3));
        // A click on the header row (y=1) is above the body -> None.
        assert_eq!(locate_diff_click(&ul, &d, &view, 5, 1), None);
        // A click well past a line's end clamps the column to the line length ("line 0").
        let (line, c0) = locate_diff_click(&ul, &d, &view, 79, 2).unwrap();
        assert_eq!((line, c0), (0, "line 0".len()), "column clamps to the line length");

        // Side-by-side: the SAME logical context line resolves from either pane at a row.
        let sl = sbs_layout();
        assert_eq!(locate_diff_click(&sl, &d, &view, 3, 4).map(|(l, _)| l), Some(2), "left pane row -> line 2");
        assert_eq!(locate_diff_click(&sl, &d, &view, 50, 4).map(|(l, _)| l), Some(2), "right pane row -> line 2");
    }

    #[test]
    fn diff_selected_text_slices_the_selected_columns_in_order() {
        let d = diff("alpha\nbravo\ncharlie\ndelta", "alpha\nbravo\ncharlie\ndelta");
        let mut view = ViewState::new(0);
        let sel = |a: (usize, usize), c: (usize, usize)| crate::view_state::DetailSel { anchor: a, cursor: c };
        // A full multi-line span: line 1 from col 0 to line 2's last column.
        view.diff_sel = Some(sel((1, 0), (2, 7)));
        assert_eq!(selected_text(&d, &view).as_deref(), Some("bravo\ncharlie"));
        // A reversed drag yields the same ordered text.
        view.diff_sel = Some(sel((2, 7), (1, 0)));
        assert_eq!(selected_text(&d, &view).as_deref(), Some("bravo\ncharlie"));
        // A single-line partial selection slices just those columns.
        view.diff_sel = Some(sel((0, 1), (0, 4)));
        assert_eq!(selected_text(&d, &view).as_deref(), Some("lph"), "alpha[1..4]");
        // A multi-line partial: tail of line 1, all of line 2, head of line 3.
        view.diff_sel = Some(sel((1, 3), (3, 2)));
        assert_eq!(selected_text(&d, &view).as_deref(), Some("vo\ncharlie\nde"));
        // An EMPTY selection (a bare click, anchor == cursor) copies nothing + bands nothing.
        view.diff_sel = Some(sel((2, 2), (2, 2)));
        assert_eq!(selected_text(&d, &view), None, "a bare click selects no text");
        assert_eq!(diff_sel_range(&view, 2, 7), None, "an empty selection bands no chars");
        // No selection -> None.
        view.diff_sel = None;
        assert_eq!(selected_text(&d, &view), None);
    }

    #[test]
    fn diff_sel_range_clips_per_line_like_an_editor() {
        let mut view = ViewState::new(0);
        // Select (1,2)..(3,4): line 1 from col 2 to end, line 2 whole, line 3 up to col 4.
        view.diff_sel = Some(crate::view_state::DetailSel { anchor: (1, 2), cursor: (3, 4) });
        assert_eq!(diff_sel_range(&view, 0, 10), None, "line above the selection");
        assert_eq!(diff_sel_range(&view, 1, 10), Some((2, 10)), "first line: anchor col -> end");
        assert_eq!(diff_sel_range(&view, 2, 8), Some((0, 8)), "interior line: whole line");
        assert_eq!(diff_sel_range(&view, 3, 10), Some((0, 4)), "last line: start -> cursor col");
        assert_eq!(diff_sel_range(&view, 4, 10), None, "line below the selection");
    }

    #[test]
    fn edit_scroll_bounds_pins_last_row_and_clamps_override_per_regime() {
        // 40 lines overflow the 20-row body; the window equals the body height (20) in
        // every regime - covers the four regimes (side-by-side/unified x wrapped/flat) the
        // wheel/render/hit-test all read.
        let src = (0..40).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let d = diff(&src, &src);
        let mut e = EditorState::opening("f".to_string());
        e.load(&src);
        let last = e.lines.len() - 1;
        // Flat-regime maxima are exact: the bottom-pinned top is n - window (window 20).
        let sbs_n = pair_rows(&d).1.len();
        let uni_n = d.lines.len();

        for (name, dl, flat_max) in [
            ("side-by-side", sbs_layout(), sbs_n.saturating_sub(20)),
            ("unified", unified_layout(), uni_n.saturating_sub(20)),
        ] {
            for &wrap in &[false, true] {
                let tag = format!("{name} wrap={wrap}");
                let mut view = ViewState::new(0);
                view.word_wrap = wrap;
                // Caret at the top -> cursor-follow top 0; 40 rows overflow 20 -> max > 0.
                e.cursor_row = 0;
                e.cursor_col = 0;
                let (cur, max) = edit_scroll_bounds(&dl, &d, &e, &view).unwrap();
                assert_eq!(cur, 0, "{tag}: caret at row 0 -> follow top 0");
                assert!(max > 0, "{tag}: 40 rows overflow a 20-row body");
                if !wrap {
                    assert_eq!(max, flat_max, "{tag}: max pins the last row at n - window");
                }
                // Caret on the LAST row -> cursor-follow pins it to the bottom (cur == max).
                e.cursor_row = last;
                let (cur, _) = edit_scroll_bounds(&dl, &d, &e, &view).unwrap();
                assert_eq!(cur, max, "{tag}: caret on the last row sits at max");
                // A free-scroll override past the end clamps to max; an in-range one is verbatim.
                view.edit_scroll = Some(usize::MAX);
                let (cur, _) = edit_scroll_bounds(&dl, &d, &e, &view).unwrap();
                assert_eq!(cur, max, "{tag}: stale override clamps to the bottom-pinned max");
                view.edit_scroll = Some(2);
                let (cur, _) = edit_scroll_bounds(&dl, &d, &e, &view).unwrap();
                assert_eq!(cur, 2, "{tag}: an in-range override is honored verbatim");
            }
        }
    }

    #[test]
    fn click_on_new_side_maps_to_buffer_row_col() {
        // 3 identical lines -> new-side rows 0,1,2 carry buffer lines 0,1,2.
        let d = diff("a\nbb\nccc\n", "a\nbb\nccc\n");
        let mut e = EditorState::opening("f".to_string());
        e.load("a\nbb\nccc\n");
        let dl = sbs_layout();
        let view = ViewState::new(0);
        // Click on the 2nd visible row (y=3), past the gutter (x = 40 + GUTTER_W + 1).
        let (row, col) = locate_edit_click(&dl, &d, &e, &view, 40 + GUTTER_W + 1, 3).unwrap();
        assert_eq!(row, 1, "second new-side row is buffer line 1");
        assert_eq!(col, 1, "one char past the gutter is col 1");
    }

    #[test]
    fn click_col_clamps_past_end_of_line() {
        let d = diff("a\n", "a\n");
        let mut e = EditorState::opening("f".to_string());
        e.load("a\n");
        let dl = sbs_layout();
        let view = ViewState::new(0);
        // Click far to the right of a 1-char line -> clamped to the line end (col 1).
        let (row, col) = locate_edit_click(&dl, &d, &e, &view, 78, 2).unwrap();
        assert_eq!((row, col), (0, 1));
    }

    #[test]
    fn click_outside_new_side_is_none() {
        let d = diff("a\n", "a\n");
        let mut e = EditorState::opening("f".to_string());
        e.load("a\n");
        let dl = sbs_layout();
        let view = ViewState::new(0);
        // A click in the OLD pane (x < 40) is not editable.
        assert!(locate_edit_click(&dl, &d, &e, &view, 5, 2).is_none());
    }

    #[test]
    fn nowrap_hscroll_follows_caret_else_override() {
        // A long line (100 chars) so the longest-line clamp (max = 100 - 20 = 80) does
        // not interfere with the offsets under test.
        let long: String = std::iter::repeat_n('x', 100).collect();
        let d = diff(&format!("{long}\n"), &format!("{long}\n"));
        let mut view = ViewState::new(0);
        // Browsing (no caret): column 0 until an override is parked.
        assert_eq!(nowrap_hscroll(&view, &d, None, 20), 0);
        // Editing: a caret inside the code width stays at 0; one past the right edge
        // scrolls just enough to keep it in the last visible column (30 - (20 - 1)).
        assert_eq!(nowrap_hscroll(&view, &d, Some(5), 20), 0);
        assert_eq!(nowrap_hscroll(&view, &d, Some(30), 20), 11);
        // A parked override wins over the caret-follow, in both modes.
        view.diff_hscroll = Some(7);
        assert_eq!(nowrap_hscroll(&view, &d, Some(30), 20), 7);
        assert_eq!(nowrap_hscroll(&view, &d, None, 20), 7);
        // A stale override past the longest line clamps so the code can never blank.
        view.diff_hscroll = Some(usize::MAX);
        assert_eq!(nowrap_hscroll(&view, &d, None, 20), 80, "clamped to longest - code_w");
    }

    #[test]
    fn hscroll_bounds_caps_at_longest_line_and_disables_under_wrap() {
        let long: String = std::iter::repeat_n('x', 100).collect();
        let src = format!("{long}\nshort\n");
        let d = diff(&src, &src);
        let dl = sbs_layout(); // body_new width 40 -> code_w = 40 - GUTTER_W(5) = 35
        let mut view = ViewState::new(0);
        // Browsing, no override -> cur 0; max = 100 - 35 = 65.
        let (cur, max) = hscroll_bounds(&dl, &d, &view, None).unwrap();
        assert_eq!((cur, max), (0, 65));
        // A stale override clamps to the longest line's overflow.
        view.diff_hscroll = Some(usize::MAX);
        let (cur, _) = hscroll_bounds(&dl, &d, &view, None).unwrap();
        assert_eq!(cur, 65);
        // Word-wrap on -> no sideways scroll at all.
        view.word_wrap = true;
        assert!(hscroll_bounds(&dl, &d, &view, None).is_none());
    }

    #[test]
    fn click_col_adds_the_horizontal_offset() {
        let long: String = std::iter::repeat_n('x', 60).collect();
        let src = format!("{long}\n");
        let d = diff(&src, &src);
        let mut e = EditorState::opening("f".to_string());
        e.load(&src);
        let dl = sbs_layout();
        let mut view = ViewState::new(0);
        view.diff_hscroll = Some(10);
        // Click the first code cell (x = 40 + GUTTER_W) on row 0: the parked offset
        // shifts the resolved column by 10.
        let (row, col) = locate_edit_click(&dl, &d, &e, &view, 40 + GUTTER_W, 2).unwrap();
        assert_eq!((row, col), (0, 10));
    }

    #[test]
    fn wrapped_click_maps_to_the_logical_row_below_a_wrapped_line() {
        // New-side row 0 is a long line that wraps to 2 physical rows in a 39-col pane;
        // row 1 is "tail". A click on the 3rd physical row (y=4) must land on buffer
        // line 1 - not line 2 - because the walk honors the first line's wrapped height.
        let long: String = std::iter::repeat_n('x', 80).collect();
        let src = format!("{long}\ntail\n");
        let d = diff(&src, &src);
        let mut e = EditorState::opening("f".to_string());
        e.load(&src);
        let dl = sbs_layout(); // body_new x=40 width=40 -> code_w 39 -> 80 chars = 3 rows
        let mut view = ViewState::new(0);
        view.word_wrap = true;
        // y=2 first content row (line 0 row 0); the long line wraps over y=2,3,4; line 1
        // starts at y=5. A click at y=4 is still inside line 0's wrap.
        let (row, _) = locate_edit_click(&dl, &d, &e, &view, 41, 4).unwrap();
        assert_eq!(row, 0, "a click on a wrapped sub-row stays on the wrapped logical line");
        let (row, _) = locate_edit_click(&dl, &d, &e, &view, 41, 5).unwrap();
        assert_eq!(row, 1, "the row after the 3-row wrap is buffer line 1");
    }
}
